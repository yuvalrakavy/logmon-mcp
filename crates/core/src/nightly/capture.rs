use super::Selector;
use crate::cases::{self, document as doc};
use crate::daemon::domain::Domain;
use crate::gelf::message::{Level, LogEntry};
use crate::span::types::{SpanEntry, SpanStatus};
use crate::store::memory::RetainedRing;
use chrono::{DateTime, Utc};
use logmon_broker_protocol::EvidenceVerdict;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// One aggregate evidence budget per domain, including per-case indexes. Records
// used by several pending or writing cases are charged once until their last
// snapshot releases them; the ordinary bounded rings may meanwhile roll over.
const CAPTURE_POOL_BYTES: usize = 128 * 1024 * 1024;
const INDEX_BYTES: usize = 96;

pub(crate) struct CapturePool {
    available: usize,
    logs: HashMap<u64, Pinned<LogEntry>>,
    spans: HashMap<u64, Pinned<SpanEntry>>,
}
impl Default for CapturePool {
    fn default() -> Self {
        Self {
            available: CAPTURE_POOL_BYTES,
            logs: HashMap::new(),
            spans: HashMap::new(),
        }
    }
}
#[cfg(test)]
impl CapturePool {
    pub(crate) fn set_available(&mut self, bytes: usize) {
        self.available = bytes;
    }
    pub(super) fn available(&self) -> usize {
        self.available
    }
}
struct Pinned<T> {
    record: Arc<T>,
    bytes: usize,
    users: usize,
}
fn pin<T>(
    records: &mut HashMap<u64, Pinned<T>>,
    available: &mut usize,
    seq: u64,
    bytes: impl FnOnce() -> usize,
    record: impl FnOnce() -> Arc<T>,
) -> Option<Arc<T>> {
    if let Some(pin) = records.get_mut(&seq) {
        *available = available.checked_sub(INDEX_BYTES)?;
        pin.users += 1;
        return Some(pin.record.clone());
    }
    let bytes = bytes();
    *available = available.checked_sub(bytes.checked_add(INDEX_BYTES)?)?;
    let record = record();
    records.insert(
        seq,
        Pinned {
            record: record.clone(),
            bytes,
            users: 1,
        },
    );
    Some(record)
}
fn unpin<T>(records: &mut HashMap<u64, Pinned<T>>, seq: u64) -> usize {
    let Some(pin) = records.get_mut(&seq) else {
        return 0;
    };
    pin.users -= 1;
    if pin.users == 0 {
        INDEX_BYTES + records.remove(&seq).expect("present").bytes
    } else {
        INDEX_BYTES
    }
}

pub struct CaptureSnapshot {
    domain: String,
    selector: Selector,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    captured_at: DateTime<Utc>,
    // A unique sequence for the end-test meta-record included in the archive.
    anchor_seq: u64,
    logs: BTreeMap<u64, Arc<LogEntry>>,
    spans: BTreeMap<u64, Arc<SpanEntry>>,
    log_floor: u64,
    span_floor: u64,
    domain_drops: Value,
    ring_coverage: Value,
    pool: Arc<Mutex<CapturePool>>,
    over_budget: bool,
}

pub(super) fn coverage(
    logs: &RetainedRing<LogEntry>,
    spans: &RetainedRing<SpanEntry>,
    cutoff: u64,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Value {
    let ring = |floor: u64, latest: Option<DateTime<Utc>>| {
        let status = match latest {
            _ if floor == 0 => "retained",
            Some(at) if at < start => "retained",
            Some(at) if at <= end => "evicted",
            _ => "unknown",
        };
        json!({"lost_below_seq":floor,"through_seq":cutoff,
            "latest_evicted_event_end":latest,"status":status})
    };
    json!({"log_coverage":ring(logs.lost_below, logs.latest_evicted),
        "span_coverage":ring(spans.lost_below, spans.latest_evicted),"coverage_as_of":Utc::now()})
}

impl Drop for CaptureSnapshot {
    fn drop(&mut self) {
        let mut pool = self.pool.lock().unwrap();
        for seq in self.logs.keys() {
            pool.available += unpin(&mut pool.logs, *seq);
        }
        for seq in self.spans.keys() {
            pool.available += unpin(&mut pool.spans, *seq);
        }
    }
}

impl CaptureSnapshot {
    pub fn new(
        domain: &Domain,
        selector: Selector,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Self {
        let anchor_seq = domain.pipeline.assign_seq();
        let drops = domain.metrics.snapshot();
        let mut snapshot = Self {
            domain: domain.config.name.to_string(),
            selector,
            start,
            end,
            captured_at: Utc::now(),
            anchor_seq,
            logs: BTreeMap::new(),
            spans: BTreeMap::new(),
            log_floor: 0,
            span_floor: 0,
            ring_coverage: Value::Null,
            domain_drops: json!({"gelf_udp":drops.gelf_udp,"gelf_tcp":drops.gelf_tcp,
                "otlp_http_logs":drops.otlp_http_logs,"otlp_http_traces":drops.otlp_http_traces,
                "otlp_grpc_logs":drops.otlp_grpc_logs,"otlp_grpc_traces":drops.otlp_grpc_traces}),
            pool: domain.test_log.capture_pool.clone(),
            over_budget: false,
        };
        // Always lock logs before spans. Both stores remain fixed while the verdict
        // snapshot is copied; regular ingestion needs only its existing ring lock.
        domain.pipeline.with_retained(|logs| {
            domain.span_store.with_retained(|spans| {
                snapshot.log_floor = logs.lost_below;
                snapshot.span_floor = spans.lost_below;
                for log in logs.records {
                    if log.seq < anchor_seq {
                        snapshot.pin_log(log, || log.clone());
                    }
                }
                for span in spans.records {
                    if span.seq < anchor_seq {
                        snapshot.pin_span(span, || span.clone());
                    }
                }
                snapshot.ring_coverage = coverage(&logs, &spans, anchor_seq, start, end);
            })
        });
        snapshot
    }

    pub fn is_over_budget(&self) -> bool {
        self.over_budget
    }

    pub fn observe_log(&mut self, log: &LogEntry) {
        self.pin_log(log, || Arc::new(log.clone()));
    }
    fn pin_log(&mut self, log: &LogEntry, get: impl FnOnce() -> Arc<LogEntry>) {
        if self.over_budget
            || self.logs.contains_key(&log.seq)
            || !(self.selector.matches_log(log) || (self.start..=self.end).contains(&log.timestamp))
        {
            return;
        }
        let mut pool = self.pool.lock().unwrap();
        let CapturePool {
            logs, available, ..
        } = &mut *pool;
        match pin(logs, available, log.seq, || log_bytes(log), get) {
            Some(record) => {
                self.logs.insert(log.seq, record);
            }
            None => {
                self.over_budget = true;
            }
        }
    }
    pub fn observe_span(&mut self, span: &SpanEntry) {
        self.pin_span(span, || Arc::new(span.clone()));
    }
    fn pin_span(&mut self, span: &SpanEntry, get: impl FnOnce() -> Arc<SpanEntry>) {
        if self.over_budget
            || self.spans.contains_key(&span.seq)
            || !(self.selector.matches_span(span)
                || (span.start_time <= self.end && span.end_time >= self.start))
        {
            return;
        }
        let mut pool = self.pool.lock().unwrap();
        let CapturePool {
            spans, available, ..
        } = &mut *pool;
        match pin(spans, available, span.seq, || span_bytes(span), get) {
            Some(record) => {
                self.spans.insert(span.seq, record);
            }
            None => {
                self.over_budget = true;
            }
        }
    }
    pub fn captured_at(&self) -> DateTime<Utc> {
        self.captured_at
    }

    pub fn coverage(&self) -> Value {
        let mut result = self.ring_coverage.clone();
        result["coverage_as_of"] = json!(self.captured_at);
        result["receiver_drops_domain_lifetime"] = self.domain_drops.clone();
        result["over_budget"] = json!(self.over_budget);
        result
    }

    fn render_summary(
        &self,
        body: &mut String,
        test_name: &str,
        reason: &str,
        message: &str,
        meta: &Value,
    ) {
        use crate::render::escape::{cell, flatten};
        use std::fmt::Write;
        let value = |v: &Value| cell(v.as_str().unwrap_or("unknown"));
        writeln!(
            body,
            "# Test execution

{} · {}
",
            flatten(test_name),
            value(&meta["verdict"])
        )
        .unwrap();
        writeln!(
            body,
            "Selector: {} = {}
",
            flatten(&self.selector.field),
            flatten(&self.selector.value)
        )
        .unwrap();
        writeln!(
            body,
            "Capture reason: {}
",
            flatten(reason)
        )
        .unwrap();
        let end_time = meta.get("ended_at").and_then(Value::as_str);
        writeln!(
            body,
            "Started at {}. End: {}. Captured at {}.
",
            self.start.to_rfc3339(),
            end_time.unwrap_or("unknown"),
            self.captured_at.to_rfc3339()
        )
        .unwrap();
        if end_time.is_some() {
            let estimate = meta
                .get("estimated_ms")
                .and_then(Value::as_u64)
                .map(|v| format!("{v} ms"))
                .unwrap_or_else(|| "unknown".into());
            writeln!(
                body,
                "Elapsed: {} ms. Estimate: {}.
",
                (self.end - self.start).num_milliseconds(),
                estimate
            )
            .unwrap();
        }
        writeln!(
            body,
            "## Reported outcome
"
        )
        .unwrap();
        if let Some(fields) = meta.pointer("/end/fields") {
            for key in ["failed_test", "file", "line"] {
                if let Some(v) = fields.get(key) {
                    let text = v
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string());
                    writeln!(body, "{}: {}", key, flatten(&text)).unwrap();
                }
            }
            writeln!(body).unwrap();
        }
        // Full text and arbitrary fields remain verbatim in the JSONL capture marker.
        for line in message.lines().take(12) {
            writeln!(body, "    {}", line.chars().take(640).collect::<String>()).unwrap();
        }
        let peers = meta["peers"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        writeln!(
            body,
            "
## Overlapping executions ({})
",
            peers.len()
        )
        .unwrap();
        if !peers.is_empty() {
            writeln!(
                body,
                "| Test | Execution ID | Selector value | Started | End |
|---|---|---|---|---|"
            )
            .unwrap();
            for peer in peers.iter().take(40) {
                let ended =
                    peer["ended_at"]
                        .as_str()
                        .unwrap_or(if peer["active_at_capture"] == true {
                            "active at capture"
                        } else {
                            "unknown"
                        });
                writeln!(
                    body,
                    "| {} | {} | {} | {} | {} |",
                    value(&peer["name"]),
                    value(&peer["execution_id"]),
                    value(&peer["selector"]["value"]),
                    value(&peer["started_at"]),
                    cell(ended)
                )
                .unwrap();
            }
        }
        let samples = self.logs.values().filter(|record| {
            record
                .additional_fields
                .get("meta_log")
                .and_then(|v| v.as_str())
                == Some("machine-load")
                && record.additional_fields.get("run_id") == meta.get("run_id")
        });
        writeln!(body, "\n## Machine samples ({})\n", samples.clone().count()).unwrap();
        if let Some(sample) = samples.max_by_key(|record| (record.timestamp, record.seq)) {
            if let Some(fields) = sample.additional_fields.get("sample") {
                writeln!(
                    body,
                    "Latest retained sample from {} at {}:\n\n    {}\n",
                    sample.host,
                    sample.timestamp,
                    fields.to_string().chars().take(1000).collect::<String>()
                )
                .unwrap();
            }
        }
        let mut slow: Vec<_> = self
            .spans
            .values()
            .filter(|s| self.selector.matches_span(s))
            .collect();
        slow.sort_by(|a, b| b.duration_ms.total_cmp(&a.duration_ms));
        if !slow.is_empty() {
            writeln!(
                body,
                "## Slowest target spans

| Span | Duration (ms) |
|---|---:|"
            )
            .unwrap();
            for span in slow.iter().take(5) {
                writeln!(body, "| {} | {:.3} |", cell(&span.name), span.duration_ms).unwrap();
            }
        }
        writeln!(
            body,
            "
## Evidence coverage

{} logs (including the capture marker) and {} spans.

    {}

Sequence {} contains the full lifecycle record, peer list and retention details in the log evidence.
",
            self.logs.len(),
            self.spans.len(),
            self.coverage(),
            self.anchor_seq
        )
        .unwrap();
    }

    /// Call only on a blocking worker. Return the path only after finishing the ZIP.
    pub fn write(
        mut self,
        case_dir: &Path,
        metadata: &Value,
        registry: Vec<doc::RegistryFact>,
    ) -> Result<PathBuf, String> {
        if self.over_budget {
            return Err("domain capture pool byte budget exhausted".into());
        }
        if self.logs.len() + self.spans.len() + 1 > cases::load::MAX_CASE_RECORDS {
            return Err("capture exceeds the standard case reader's record limit".into());
        }
        let test_name = metadata
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&self.selector.value)
            .to_owned();
        let prefix = cases::resolve_prefix(Some(&test_name), None, &self.domain);
        let file =
            cases::claim_bundle(case_dir, &prefix, self.captured_at).map_err(|e| e.to_string())?;
        let path = file.path.clone();
        let result = (|| {
            let reason = metadata
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("test capture");
            let end = metadata
                .get("end_event")
                .or_else(|| metadata.get("end"))
                .or_else(|| metadata.get("end_test"))
                .unwrap_or(metadata);
            let headline = end
                .get("message")
                .or_else(|| end.pointer("/fields/message"))
                .and_then(Value::as_str)
                .unwrap_or(reason);
            let mut marker = LogEntry::synthetic(Level::Info, headline);
            marker.seq = self.anchor_seq;
            marker.timestamp = self.end;
            marker.host = "logmon".into();
            marker.trace_id = (self.selector.field == "trace_id")
                .then(|| u128::from_str_radix(&self.selector.value, 16).ok())
                .flatten();
            marker
                .additional_fields
                .insert("meta_log".into(), json!("end-test-capture"));
            marker
                .additional_fields
                .insert("case_context".into(), metadata.clone());
            marker
                .additional_fields
                .insert("coverage".into(), self.coverage());
            self.logs.insert(marker.seq, Arc::new(marker));
            let first = self
                .logs
                .keys()
                .next()
                .into_iter()
                .chain(self.spans.keys().next())
                .copied()
                .min()
                .unwrap_or(self.anchor_seq);
            let last = self
                .logs
                .keys()
                .next_back()
                .into_iter()
                .chain(self.spans.keys().next_back())
                .copied()
                .max()
                .unwrap_or(self.anchor_seq);
            let names = cases::file_names(&file.stem);
            let input = doc::CaseInput {
                stem: file.stem.clone(),
                captured_at: self.captured_at,
                domain: self.domain.clone(),
                incarnation: metadata
                    .get("incarnation")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                reason: reason.into(),
                anchor: doc::Anchor {
                    kind: "seq",
                    label: self.anchor_seq.to_string(),
                    seq: self.anchor_seq,
                    at: self.end,
                    message: headline.to_string(),
                    of_many: None,
                },
                // A selection by time/selector is sparse on the domain's shared
                // sequence axis. Do not grade the intervening domain data complete.
                window: doc::Window {
                    from: first,
                    to: last,
                    verdict: EvidenceVerdict::CannotVerify,
                    narrowed_by: vec![],
                    clamped: false,
                    short_before: 0,
                    short_after: 0,
                    log_lost_below: self.log_floor,
                    logs_evicted_before_window: None,
                    spans_evicted_before_window: None,
                    span_lost_below: self.span_floor,
                },
                logdata: Some(doc::FilePointer {
                    file: names[1].clone(),
                    records: self.logs.len() as u64,
                    by_source: None,
                }),
                spandata: Some(doc::FilePointer {
                    file: names[2].clone(),
                    records: self.spans.len() as u64,
                    by_source: None,
                }),
                registry,
                asserted: vec![],
                neighbours: vec![],
                spans: vec![],
                collectors: vec![],
            };
            let mut body = String::new();
            doc::front_matter(&mut body, &input);
            self.render_summary(&mut body, &test_name, reason, headline, metadata);
            let registry_name = cases::bundle::registry_entry_name(&file.stem);
            let entries = [
                names[0].as_str(),
                names[1].as_str(),
                names[2].as_str(),
                registry_name.as_str(),
            ];
            cases::bundle::pack_stream(
                file.file,
                &file.stem,
                entries.into_iter().enumerate().map(|(i, n)| (n, i)),
                true,
                |zip, i| {
                    match i {
                        0 => zip.write_all(body.as_bytes())?,
                        1 => {
                            cases::write_jsonl(
                                zip,
                                Path::new(&names[1]),
                                "logdata",
                                self.logs.values().map(Arc::as_ref),
                            )
                            .map_err(std::io::Error::other)?;
                        }
                        2 => {
                            cases::write_jsonl(
                                zip,
                                Path::new(&names[2]),
                                "spandata",
                                self.spans.values().map(Arc::as_ref),
                            )
                            .map_err(std::io::Error::other)?;
                        }
                        _ => zip.write_all(&doc::registry_json(&input.registry))?,
                    }
                    Ok(())
                },
            )
            .map_err(|e| e.to_string())?;
            Ok(path.clone())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(path);
        }
        result
    }
}

// Estimate owned snapshot memory, charging fixed map/node overhead as well as
// dynamic strings, fields and events before cloning. Avoid serializing records
// under the ring read lock.
fn attributes_bytes(fields: &HashMap<String, Value>) -> usize {
    fields
        .iter()
        .map(|(k, v)| 128 + k.len() + value_bytes(v))
        .sum()
}
fn value_bytes(v: &Value) -> usize {
    match v {
        Value::String(s) => 64 + s.len(),
        Value::Array(a) => 64 + a.iter().map(value_bytes).sum::<usize>(),
        Value::Object(m) => {
            128 + m
                .iter()
                .map(|(k, v)| 128 + k.len() + value_bytes(v))
                .sum::<usize>()
        }
        _ => 64,
    }
}
fn log_bytes(log: &LogEntry) -> usize {
    128 + size_of::<LogEntry>()
        + log.message.len()
        + log.host.len()
        + log.full_message.as_ref().map_or(0, String::len)
        + log.facility.as_ref().map_or(0, String::len)
        + log.file.as_ref().map_or(0, String::len)
        + log
            .matched_filters
            .iter()
            .map(|s| 32 + s.len())
            .sum::<usize>()
        + attributes_bytes(&log.additional_fields)
}
fn span_bytes(span: &SpanEntry) -> usize {
    128 + size_of::<SpanEntry>()
        + span.name.len()
        + span.service_name.len()
        + attributes_bytes(&span.attributes)
        + match &span.status {
            SpanStatus::Error(s) => s.len(),
            _ => 0,
        }
        + span
            .events
            .iter()
            .map(|e| 128 + e.name.len() + attributes_bytes(&e.attributes))
            .sum::<usize>()
}
