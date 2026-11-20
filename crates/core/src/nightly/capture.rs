use super::Selector;
use crate::cases::{self, document as doc};
use crate::daemon::domain::Domain;
use crate::gelf::message::{Level, LogEntry};
use crate::span::types::{SpanEntry, SpanStatus};
use crate::store::memory::RetainedRing;
use chrono::{DateTime, Utc};
use logmon_broker_protocol::EvidenceVerdict;
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    shared_bytes: usize,
    logs: HashMap<u64, Pinned<LogEntry>>,
    spans: HashMap<u64, Pinned<SpanEntry>>,
}
impl Default for CapturePool {
    fn default() -> Self {
        Self {
            available: CAPTURE_POOL_BYTES,
            shared_bytes: 0,
            logs: HashMap::new(),
            spans: HashMap::new(),
        }
    }
}
impl CapturePool {
    pub(crate) fn exclusive_bytes(&self) -> usize {
        CAPTURE_POOL_BYTES
            .saturating_sub(self.available)
            .saturating_sub(self.shared_bytes)
    }
    pub(crate) fn evict_log(&mut self, seq: u64) {
        if let Some(record) = self.logs.get_mut(&seq).filter(|r| r.shared) {
            self.shared_bytes -= record.bytes;
            record.shared = false;
        }
    }
    pub(crate) fn evict_span(&mut self, seq: u64) {
        if let Some(record) = self.spans.get_mut(&seq).filter(|r| r.shared) {
            self.shared_bytes -= record.bytes;
            record.shared = false;
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
    shared: bool,
}
fn pin<T>(
    records: &mut HashMap<u64, Pinned<T>>,
    available: &mut usize,
    seq: u64,
    shared: bool,
    shared_bytes: &mut usize,
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
    if shared {
        *shared_bytes += bytes;
    }
    records.insert(
        seq,
        Pinned {
            record: record.clone(),
            bytes,
            users: 1,
            shared,
        },
    );
    Some(record)
}
fn unpin<T>(records: &mut HashMap<u64, Pinned<T>>, seq: u64, shared_bytes: &mut usize) -> usize {
    let Some(pin) = records.get_mut(&seq) else {
        return 0;
    };
    pin.users -= 1;
    if pin.users == 0 {
        if pin.shared {
            *shared_bytes -= pin.bytes;
        }
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
    log_tip: Option<u64>,
    span_tip: Option<u64>,
    domain_drops: Value,
    ring_coverage: Value,
    pool: Arc<Mutex<CapturePool>>,
    pub(crate) size: CaptureSize,
    omitted: [usize; 5],
    cutoff: Option<Priority>,
    retained: BTreeSet<Priority>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureSize {
    Full,
    Minimal,
    Bare,
}

impl CaptureSize {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Minimal => "minimal",
            Self::Bare => "bare",
        }
    }
}

// One shared sequence axis gives logs and spans the same newest-first order.
type Priority = (usize, Reverse<u64>);

enum Record<'a> {
    Log(&'a LogEntry),
    Span(&'a SpanEntry),
}

impl Record<'_> {
    fn priority(&self, snapshot: &CaptureSnapshot) -> Option<Priority> {
        let (own, in_window, error, warn, seq) = match self {
            Self::Log(log) => (
                snapshot.selector.matches_log(log),
                (snapshot.start..=snapshot.end).contains(&log.timestamp),
                log.level == Level::Error,
                log.level == Level::Warn,
                log.seq,
            ),
            Self::Span(span) => (
                snapshot.selector.matches_span(span),
                span.start_time <= snapshot.end && span.end_time >= snapshot.start,
                matches!(span.status, SpanStatus::Error(_)),
                false,
                span.seq,
            ),
        };
        let class = if own {
            usize::from(!error)
        } else if !in_window || snapshot.size == CaptureSize::Minimal {
            return None;
        } else if error {
            2
        } else if warn {
            3
        } else {
            4
        };
        Some((class, Reverse(seq)))
    }
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
        if self.logs.is_empty() && self.spans.is_empty() {
            return;
        }
        let mut pool = self.pool.lock().unwrap();
        let CapturePool {
            logs,
            spans,
            available,
            shared_bytes,
        } = &mut *pool;
        for seq in self.logs.keys() {
            *available += unpin(logs, *seq, shared_bytes);
        }
        for seq in self.spans.keys() {
            *available += unpin(spans, *seq, shared_bytes);
        }
    }
}

struct CaseClaim(PathBuf);
impl Drop for CaseClaim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn zone(metadata: &Value) -> chrono_tz::Tz {
    metadata["display_timezone"]
        .as_str()
        .and_then(|name| name.parse().ok())
        .unwrap_or(chrono_tz::UTC)
}

impl CaptureSnapshot {
    pub fn new(
        domain: &Domain,
        selector: Selector,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Self {
        Self::capture(domain, selector, start, end, CaptureSize::Full)
    }

    fn capture(
        domain: &Domain,
        selector: Selector,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        size: CaptureSize,
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
            log_tip: None,
            span_tip: None,
            ring_coverage: Value::Null,
            domain_drops: json!({"gelf_udp":drops.gelf_udp,"gelf_tcp":drops.gelf_tcp,
                "otlp_http_logs":drops.otlp_http_logs,"otlp_http_traces":drops.otlp_http_traces,
                "otlp_grpc_logs":drops.otlp_grpc_logs,"otlp_grpc_traces":drops.otlp_grpc_traces}),
            pool: domain.test_log.capture_pool.clone(),
            size,
            omitted: [0; 5],
            cutoff: None,
            retained: BTreeSet::new(),
        };
        // Always lock logs before spans. Both stores remain fixed while the verdict
        // snapshot is copied; regular ingestion needs only its existing ring lock.
        domain.pipeline.with_retained(|logs| {
            domain.span_store.with_retained(|spans| {
                snapshot.log_floor = logs.lost_below;
                snapshot.span_floor = spans.lost_below;
                // Each stream stores a record before its single processor calls
                // into the run. At most its newest captured record can still
                // have an observation waiting behind the verdict's state lock.
                snapshot.log_tip = logs
                    .records
                    .iter()
                    .rev()
                    .find(|log| log.seq < anchor_seq)
                    .map(|log| log.seq);
                snapshot.span_tip = spans
                    .records
                    .iter()
                    .rev()
                    .find(|span| span.seq < anchor_seq)
                    .map(|span| span.seq);
                snapshot.capture_records(&logs, &spans);
                snapshot.ring_coverage = coverage(&logs, &spans, anchor_seq, start, end);
            })
        });
        if size != CaptureSize::Bare
            && snapshot.retained.is_empty()
            && (snapshot.cutoff.is_some() || snapshot.pool.lock().unwrap().available < INDEX_BYTES)
        {
            snapshot.size = CaptureSize::Bare;
        }
        snapshot
    }

    fn capture_records(&mut self, logs: &RetainedRing<LogEntry>, spans: &RetainedRing<SpanEntry>) {
        if self.size == CaptureSize::Bare {
            for record in logs
                .records
                .iter()
                .map(|r| Record::Log(r))
                .chain(spans.records.iter().map(|r| Record::Span(r)))
            {
                if let Some((class, Reverse(seq))) = record.priority(self) {
                    if seq < self.anchor_seq {
                        self.omitted[class] += 1;
                    }
                }
            }
            return;
        }
        // Stream the two rings in arrival order for each priority class.
        // This keeps the temporary work independent of the number of candidates.
        let classes = if self.size == CaptureSize::Minimal {
            2
        } else {
            5
        };
        for class in 0..classes {
            let mut logs = logs.records.iter().rev().peekable();
            let mut spans = spans.records.iter().rev().peekable();
            loop {
                if logs
                    .peek()
                    .is_some_and(|log| spans.peek().is_none_or(|span| log.seq > span.seq))
                {
                    let log = logs.next().unwrap();
                    if let Some(priority) = Record::Log(log)
                        .priority(self)
                        .filter(|key| key.0 == class && log.seq < self.anchor_seq)
                    {
                        self.offer(priority, |snap| snap.pin_log(log, true, || log.clone()));
                    }
                } else if let Some(span) = spans.next() {
                    if let Some(priority) = Record::Span(span)
                        .priority(self)
                        .filter(|key| key.0 == class && span.seq < self.anchor_seq)
                    {
                        self.offer(priority, |snap| snap.pin_span(span, true, || span.clone()));
                    }
                } else {
                    break;
                }
            }
        }
    }

    /// Preserve the domain through the broker cutoff, including activity
    /// between the runner verdict and its delivery. Subsequent grace arrivals
    /// qualify by selector or by their original execution window.
    pub fn for_execution(
        domain: &Domain,
        selector: Selector,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Self {
        Self::for_execution_size(domain, selector, start, end, CaptureSize::Full)
    }

    pub(crate) fn for_execution_size(
        domain: &Domain,
        selector: Selector,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        size: CaptureSize,
    ) -> Self {
        let mut snapshot = Self::capture(domain, selector, start, end.max(Utc::now()), size);
        snapshot.end = end;
        snapshot
    }

    /// Transfer owned pool references into one shared artifact. Duplicate pins
    /// remain with the consumed snapshot and are released by its Drop.
    pub fn merge(&mut self, mut other: Self) {
        other.logs.retain(|seq, log| match self.logs.entry(*seq) {
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(log.clone());
                false
            }
            _ => true,
        });
        other
            .spans
            .retain(|seq, span| match self.spans.entry(*seq) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(span.clone());
                    false
                }
                _ => true,
            });
        for (count, member) in self.omitted.iter_mut().zip(other.omitted) {
            *count += member;
        }
        self.start = self.start.min(other.start);
        self.end = self.end.max(other.end);
    }

    pub fn is_over_budget(&self) -> bool {
        self.omitted.iter().any(|&count| count != 0)
    }

    pub fn observe_log(&mut self, log: &LogEntry) {
        if self.log_tip == Some(log.seq) {
            return;
        }
        if let Some(priority) = Record::Log(log).priority(self) {
            self.offer(priority, |snap| {
                snap.pin_log(log, false, || Arc::new(log.clone()))
            });
        }
    }
    fn pin_log(
        &mut self,
        log: &LogEntry,
        shared: bool,
        get: impl FnOnce() -> Arc<LogEntry>,
    ) -> bool {
        let mut pool = self.pool.lock().unwrap();
        let CapturePool {
            logs,
            available,
            shared_bytes,
            ..
        } = &mut *pool;
        match pin(
            logs,
            available,
            log.seq,
            shared,
            shared_bytes,
            || log_bytes(log),
            get,
        ) {
            Some(record) => {
                self.logs.insert(log.seq, record);
                true
            }
            None => false,
        }
    }
    pub fn observe_span(&mut self, span: &SpanEntry) {
        if self.span_tip == Some(span.seq) {
            return;
        }
        if let Some(priority) = Record::Span(span).priority(self) {
            self.offer(priority, |snap| {
                snap.pin_span(span, false, || Arc::new(span.clone()))
            });
        }
    }
    fn pin_span(
        &mut self,
        span: &SpanEntry,
        shared: bool,
        get: impl FnOnce() -> Arc<SpanEntry>,
    ) -> bool {
        let mut pool = self.pool.lock().unwrap();
        let CapturePool {
            spans,
            available,
            shared_bytes,
            ..
        } = &mut *pool;
        match pin(
            spans,
            available,
            span.seq,
            shared,
            shared_bytes,
            || span_bytes(span),
            get,
        ) {
            Some(record) => {
                self.spans.insert(span.seq, record);
                true
            }
            None => false,
        }
    }

    fn offer(&mut self, priority: Priority, pin: impl Fn(&mut Self) -> bool) {
        if self.retained.contains(&priority) {
            return;
        }
        if self.size == CaptureSize::Bare || self.cutoff.is_some_and(|cutoff| priority >= cutoff) {
            self.omitted[priority.0] += 1;
            return;
        }
        // A late record can outrank evidence already pinned during the verdict.
        loop {
            if pin(self) {
                self.retained.insert(priority);
                return;
            }
            let Some(worst) = self.retained.last().copied().filter(|&key| key > priority) else {
                self.omit(priority);
                return;
            };
            self.retained.remove(&worst);
            self.omit(worst);
            let seq = worst.1 .0;
            let mut pool = self.pool.lock().unwrap();
            let CapturePool {
                logs,
                spans,
                available,
                shared_bytes,
            } = &mut *pool;
            *available += if self.logs.remove(&seq).is_some() {
                unpin(logs, seq, shared_bytes)
            } else {
                self.spans.remove(&seq);
                unpin(spans, seq, shared_bytes)
            };
        }
    }

    fn omit(&mut self, priority: Priority) {
        self.omitted[priority.0] += 1;
        self.cutoff = Some(self.cutoff.map_or(priority, |cutoff| cutoff.min(priority)));
    }

    fn omission_counts(&self) -> Value {
        let [selector_error, selector_other, window_error, window_warn, window_other] =
            self.omitted;
        json!({
            "selector_error":selector_error, "selector_other":selector_other,
            "window_error":window_error, "window_warn":window_warn, "window_other":window_other,
        })
    }
    pub fn captured_at(&self) -> DateTime<Utc> {
        self.captured_at
    }

    pub fn coverage(&self) -> Value {
        let mut result = self.ring_coverage.clone();
        result["requested_window_start"] = json!(self.start);
        // Bounds describe the actual timestamps present in each saved raw stream.
        result["earliest_retained_log_time"] = json!(self.logs.values().map(|e| e.timestamp).min());
        result["earliest_retained_span_start"] =
            json!(self.spans.values().map(|s| s.start_time).min());
        result["coverage_as_of"] = json!(self.captured_at);
        result["receiver_drops_domain_lifetime"] = self.domain_drops.clone();
        let cut = self.omitted.iter().any(|&count| count != 0);
        result["over_budget"] =
            json!(cut && self.size != CaptureSize::Bare || self.cutoff.is_some());
        result["cut"] = json!(cut);
        result["omitted_records"] = self.omission_counts();
        result
    }

    fn case_coverage(&self, metadata: &Value) -> Value {
        let mut coverage = self.coverage();
        if let Some(members) = metadata["deadline_members"].as_array() {
            // Members' windows can overlap, and a record omitted from one
            // member's snapshot might be present in a peer's snapshot.
            coverage.as_object_mut().unwrap().remove("omitted_records");
            coverage["omitted_records_by_execution"] = json!(members.iter().map(|member| {
                json!({"execution_id":member["execution_id"],"records":member["coverage"]["omitted_records"]})
            }).collect::<Vec<_>>());
        }
        coverage
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

Case size: {}
",
            flatten(reason),
            self.size.as_str(),
        )
        .unwrap();
        if let Some(detail) = meta["case_detail"].as_str() {
            writeln!(body, "Case detail: {}.\n", flatten(detail)).unwrap();
        }
        let zone = zone(meta);
        let display = |v: &Value| {
            serde_json::from_value::<DateTime<Utc>>(v.clone())
                .ok()
                .map(|t| t.with_timezone(&zone).to_rfc3339())
        };
        writeln!(
            body,
            "Report timezone: {zone}. Capture in UTC: {}.\n",
            self.captured_at.to_rfc3339()
        )
        .unwrap();
        let end_time = display(&meta["ended_at"]);
        writeln!(
            body,
            "Started at {}. End: {}. Captured at {}.
",
            display(&meta["started_at"])
                .unwrap_or_else(|| self.start.with_timezone(&zone).to_rfc3339()),
            end_time.as_deref().unwrap_or("unknown"),
            self.captured_at.with_timezone(&zone).to_rfc3339()
        )
        .unwrap();
        if end_time.is_some() {
            let estimate = meta
                .get("estimated_ms")
                .and_then(Value::as_u64)
                .map(|v| format!("{v} ms"))
                .unwrap_or_else(|| "unknown".into());
            let runtime = meta
                .pointer("/end/fields/member_runtime_ms")
                .and_then(Value::as_u64);
            writeln!(
                body,
                "Chain wall duration: {} ms. {} Estimate: {}.
",
                (self.end
                    - serde_json::from_value::<DateTime<Utc>>(meta["started_at"].clone())
                        .unwrap_or(self.start))
                .num_milliseconds(),
                runtime
                    .map(|v| format!("Measured member runtime: {v} ms (estimate basis)."))
                    .unwrap_or_default(),
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
                    display(&peer["started_at"]).unwrap_or_else(|| "unknown".into()),
                    display(&peer["ended_at"]).unwrap_or_else(|| cell(ended))
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
                    sample.timestamp.with_timezone(&zone).to_rfc3339(),
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
            self.case_coverage(meta),
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
        if self.logs.len() + self.spans.len() + 1 > cases::load::MAX_CASE_RECORDS {
            return Err("capture exceeds the standard case reader's record limit".into());
        }
        let test_name = metadata
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&self.selector.value)
            .to_owned();
        let prefix = cases::resolve_prefix(
            metadata
                .pointer("/end/fields/failed_test")
                .and_then(Value::as_str)
                .or(Some(&test_name)),
            None,
            &self.domain,
        );
        let suffix = if self.selector.field == "trace_id" {
            u128::from_str_radix(&self.selector.value, 16)
                .ok()
                .map(|id| format!("{id:032x}"))
        } else {
            None
        }
        .unwrap_or_else(|| {
            cases::resolve_prefix(
                metadata["execution_id"].as_str(),
                None,
                &self.selector.value,
            )
        });
        let group = metadata["deadline_members"].as_array();
        let prefix = if group.is_some() {
            "deadline-group"
        } else {
            &prefix
        };
        let expanded = metadata["expanded_cases"] == true;
        std::fs::create_dir_all(case_dir).map_err(|e| e.to_string())?;
        let mut stem = format!("{prefix}--{suffix}");
        let (path, _claim) = loop {
            let path = if expanded {
                case_dir.join(&stem)
            } else {
                case_dir.join(cases::bundle::bundle_name(&stem))
            };
            let claim = case_dir.join(format!(".{stem}.claim"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&claim)
            {
                Ok(_) => {
                    let claim = CaseClaim(claim);
                    if !path.try_exists().map_err(|e| e.to_string())? {
                        break (path, claim);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.to_string()),
            }
            stem = format!("{prefix}-{}--{suffix}", cases::roll_id());
        };
        let stage = case_dir.join(format!(".pending-{}", uuid::Uuid::new_v4()));
        if expanded {
            std::fs::create_dir(&stage).map_err(|e| e.to_string())?;
        }
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
                .insert("coverage".into(), self.case_coverage(metadata));
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
            let names = cases::file_names(&stem);
            let input = doc::CaseInput {
                focus_traces: if let Some(members) = group {
                    members
                        .iter()
                        .filter_map(|m| {
                            (m["selector"]["field"] == "trace_id")
                                .then(|| m["selector"]["value"].as_str())
                                .flatten()
                        })
                        .filter_map(|id| u128::from_str_radix(id, 16).ok())
                        .collect()
                } else if self.selector.field == "trace_id" {
                    u128::from_str_radix(&self.selector.value, 16)
                        .ok()
                        .into_iter()
                        .collect()
                } else {
                    vec![]
                },
                stem: stem.clone(),
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
            if let Some(members) = group {
                use std::fmt::Write;
                body.push_str("\n## Deadline crossings\n\n| Execution | Name | Focus | Crossing |\n|---|---|---|---|\n");
                for m in members {
                    let field =
                        |v: &Value| crate::render::escape::cell(v.as_str().unwrap_or("unknown"));
                    writeln!(
                        body,
                        "| {} | {} | {} | {} |",
                        field(&m["execution_id"]),
                        field(&m["name"]),
                        field(&m["selector"]["value"]),
                        field(&m["captured_at"])
                    )
                    .unwrap();
                }
            }
            if !input.focus_traces.is_empty() {
                let peers: Vec<_> = metadata["peers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|p| p["selector"]["field"] == "trace_id")
                    .filter_map(|p| u128::from_str_radix(p["selector"]["value"].as_str()?, 16).ok())
                    .collect();
                cases::focus::render(
                    &mut body,
                    &input,
                    &peers,
                    zone(metadata),
                    self.logs.values().map(Arc::as_ref),
                    self.spans.values().map(Arc::as_ref),
                );
            }
            let registry_name = cases::bundle::registry_entry_name(&stem);
            let entries = [
                names[0].as_str(),
                names[1].as_str(),
                names[2].as_str(),
                registry_name.as_str(),
            ];
            let write_entry =
                |zip: &mut dyn std::io::Write, i| -> Result<(), cases::bundle::BundleError> {
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
                };
            if expanded {
                for (i, name) in entries.iter().enumerate() {
                    let mut file =
                        std::fs::File::create(stage.join(name)).map_err(|e| e.to_string())?;
                    write_entry(&mut file, i).map_err(|e| e.to_string())?;
                }
                std::fs::rename(&stage, &path).map_err(|e| e.to_string())?;
                Ok(path.join(&names[0]))
            } else {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&stage)
                    .map_err(|e| e.to_string())?;
                cases::bundle::pack_stream(
                    file,
                    &stem,
                    entries.into_iter().enumerate().map(|(i, n)| (n, i)),
                    true,
                    write_entry,
                )
                .map_err(|e| e.to_string())?;
                std::fs::rename(&stage, &path).map_err(|e| e.to_string())?;
                Ok(path.clone())
            }
        })();
        if expanded && result.is_err() {
            let _ = std::fs::remove_dir_all(&stage);
        }
        if !expanded {
            let _ = std::fs::remove_file(&stage);
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
pub(crate) fn log_bytes(log: &LogEntry) -> usize {
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
pub(crate) fn span_bytes(span: &SpanEntry) -> usize {
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
