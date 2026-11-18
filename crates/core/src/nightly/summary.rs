use crate::gelf::message::{Level, LogEntry};
use crate::span::types::SpanEntry;
use serde::Serialize;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

// The run chooses the schema once. A test that touches an unbounded number
// of stores must not allocate one key per store for the rest of the night.
const ATTR_BUDGET: usize = 32 * 1024;
const MAX_NAMES: usize = 256;
const MAX_NAME_BYTES: usize = 512;

#[derive(Clone, Debug)]
pub struct Selector {
    pub field: String,
    pub value: String,
    trace: Option<u128>,
}

impl Selector {
    pub fn new(field: &str, value: &str) -> Self {
        let trace = (field == "trace_id")
            .then(|| u128::from_str_radix(value, 16).ok())
            .flatten();
        Self {
            field: field.to_owned(),
            value: value.to_owned(),
            trace,
        }
    }

    pub fn log_value<'a>(field: &str, log: &'a LogEntry) -> Option<Cow<'a, str>> {
        if field == "trace_id" {
            return log.trace_id.map(|id| Cow::Owned(format!("{id:032x}")));
        }
        scalar(log.additional_fields.get(field)?)
    }

    pub fn span_value<'a>(field: &str, span: &'a SpanEntry) -> Option<Cow<'a, str>> {
        if field == "trace_id" {
            return Some(Cow::Owned(format!("{:032x}", span.trace_id)));
        }
        scalar(span.attributes.get(field)?)
    }

    pub fn matches_log(&self, log: &LogEntry) -> bool {
        if self.field == "trace_id" {
            self.trace.is_some() && self.trace == log.trace_id
        } else {
            matches_value(log.additional_fields.get(&self.field), &self.value)
        }
    }

    pub fn matches_span(&self, span: &SpanEntry) -> bool {
        if self.field == "trace_id" {
            self.trace == Some(span.trace_id)
        } else {
            matches_value(span.attributes.get(&self.field), &self.value)
        }
    }
}

fn scalar(value: &Value) -> Option<Cow<'_, str>> {
    match value {
        Value::String(s) => Some(Cow::Borrowed(s)),
        Value::Number(_) | Value::Bool(_) => Some(Cow::Owned(value.to_string())),
        _ => None,
    }
}

fn matches_value(value: Option<&Value>, expected: &str) -> bool {
    value
        .and_then(scalar)
        .is_some_and(|field| field == expected)
}

#[derive(Default, Debug, Serialize)]
struct SpanTotal {
    count: u64,
    total_duration_ms: f64,
}
impl SpanTotal {
    fn add(&mut self, span: &SpanEntry) {
        self.count += 1;
        self.total_duration_ms += span.duration_ms;
    }
}

#[derive(Default, Debug, Serialize)]
struct Values {
    values: BTreeSet<ValueKey>,
    truncated: bool,
}
// JSON encoding preserves the distinction between "1", 1 and true.
#[derive(Default, Debug, Eq, PartialEq, PartialOrd, Ord)]
struct ValueKey(String);
impl Serialize for ValueKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serde_json::from_str::<Value>(&self.0)
            .map_err(serde::ser::Error::custom)?
            .serialize(s)
    }
}

struct CappedJson(Vec<u8>);
impl std::io::Write for CappedJson {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        if input.len() > ATTR_BUDGET.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("attribute exceeds summary budget"));
        }
        self.0.extend_from_slice(input);
        Ok(input.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct Summary {
    pub selector: Selector,
    levels: BTreeMap<LevelKey, u64>,
    spans: BTreeMap<String, SpanTotal>,
    other: SpanTotal,
    // A fixed bitmap estimates distinct omitted names independently of occurrences.
    omitted_names: [u64; 64],
    attrs: BTreeMap<String, Values>,
    attr_bytes: usize,
    omitted_attr_keys: usize,
    pending_duplicate_log: u64,
    pending_duplicate_span: u64,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(transparent)]
struct LevelKey(&'static str);
impl Summary {
    pub fn new(field: &str, value: &str, attributes: &[String]) -> Self {
        // Schema bytes count as well as value bytes. The full ordered list lives
        // once in begin-test-log; omitted keys can be identified by that list.
        let mut attrs = BTreeMap::new();
        let mut attr_bytes = 0usize;
        let mut omitted_attr_keys = 0;
        for key in attributes {
            let cost = key.len().saturating_add(96);
            if attrs.contains_key(key) {
                continue;
            }
            if cost <= ATTR_BUDGET.saturating_sub(attr_bytes) {
                attr_bytes += cost;
                attrs.insert(key.clone(), Values::default());
            } else {
                omitted_attr_keys += 1;
            }
        }
        Self {
            selector: Selector::new(field, value),
            levels: BTreeMap::new(),
            spans: BTreeMap::new(),
            other: SpanTotal::default(),
            omitted_names: [0; 64],
            attrs,
            attr_bytes,
            omitted_attr_keys,
            pending_duplicate_log: 0,
            pending_duplicate_span: 0,
        }
    }

    pub fn observe_log(&mut self, log: &LogEntry) {
        if (self.pending_duplicate_log == 0 || log.seq != self.pending_duplicate_log)
            && self.selector.matches_log(log)
        {
            let level = match log.level {
                Level::Trace => "TRACE",
                Level::Debug => "DEBUG",
                Level::Info => "INFO",
                Level::Warn => "WARN",
                Level::Error => "ERROR",
            };
            *self.levels.entry(LevelKey(level)).or_default() += 1;
        }
    }

    pub fn observe_span(&mut self, span: &SpanEntry) {
        if (self.pending_duplicate_span != 0 && span.seq == self.pending_duplicate_span)
            || !self.selector.matches_span(span)
        {
            return;
        }
        if let Some(row) = self.spans.get_mut(&span.name) {
            row.add(span);
        } else if self.spans.len() < MAX_NAMES && span.name.len() <= MAX_NAME_BYTES {
            self.spans.entry(span.name.clone()).or_default().add(span);
        } else {
            self.other.add(span);
            use std::hash::{Hash, Hasher};
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            span.name.hash(&mut hash);
            let bit = (hash.finish() % 4096) as usize;
            self.omitted_names[bit / 64] |= 1 << (bit % 64);
        }
        for (key, collected) in &mut self.attrs {
            if let Some(value) = span.attributes.get(key) {
                if collected.truncated {
                    continue;
                }
                // Streaming through a limited writer avoids creating a huge
                // temporary String for one pathological attribute.
                let mut writer = CappedJson(Vec::new());
                if serde_json::to_writer(&mut writer, value).is_err() {
                    collected.truncated = true;
                    continue;
                }
                let key = ValueKey(String::from_utf8(writer.0).expect("JSON is UTF-8"));
                if collected.values.contains(&key) {
                    continue;
                }
                let cost = key.0.len() + 64; // allow room for the tree node as well as the text
                if cost <= ATTR_BUDGET.saturating_sub(self.attr_bytes) {
                    self.attr_bytes += cost;
                    collected.values.insert(key);
                } else {
                    collected.truncated = true;
                }
            }
        }
    }

    /// Coverage remains available for sampled-out executions without aggregating
    /// any records.
    pub fn backfill_coverage(
        domain: &crate::daemon::domain::Domain,
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
    ) -> Value {
        domain.pipeline.with_retained(|logs| {
            domain.span_store.with_retained(|spans| {
                super::capture::coverage(&logs, &spans, domain.pipeline.current_seq(), start, end)
            })
        })
    }

    pub fn backfill(
        &mut self,
        domain: &crate::daemon::domain::Domain,
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
    ) -> Value {
        let cutoff = domain.pipeline.current_seq();
        let mut max_log = 0;
        let mut max_span = 0;
        domain.pipeline.with_retained(|logs| {
            domain.span_store.with_retained(|spans| {
                for log in logs.records {
                    if log.seq <= cutoff && (start..=end).contains(&log.timestamp) {
                        // The domain has one synchronous processor per telemetry
                        // kind. At most its current record can still call our
                        // observer after releasing the ring write lock. Machine
                        // samples enter separately under the lifecycle lock.
                        if log
                            .additional_fields
                            .get("meta_log")
                            .and_then(Value::as_str)
                            != Some("machine-load")
                        {
                            max_log = max_log.max(log.seq);
                        }
                        self.observe_log(log);
                    }
                }
                for span in spans.records {
                    if span.seq <= cutoff && span.start_time <= end && span.end_time >= start {
                        max_span = max_span.max(span.seq);
                        self.observe_span(span);
                    }
                }
                self.pending_duplicate_log = max_log;
                self.pending_duplicate_span = max_span;
                super::capture::coverage(&logs, &spans, cutoff, start, end)
            })
        })
    }

    pub fn json(&self) -> Value {
        let empty: u32 = self.omitted_names.iter().map(|x| x.count_zeros()).sum();
        let estimate =
            (empty != 0).then(|| (-4096.0 * (f64::from(empty) / 4096.0).ln()).round() as u64);
        json!({
            "spans":self.spans, "log_levels":self.levels, "distinct_values":self.attrs,
            "omitted_attribute_keys":self.omitted_attr_keys,
            "span_overflow":{"count":self.other.count,"total_duration_ms":self.other.total_duration_ms,
                "estimated_distinct_names":estimate,"estimate_saturated":empty==0},
        })
    }
}
