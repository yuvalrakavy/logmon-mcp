use super::{refused, writer::Journal};
use crate::cases::document::RegistryFact;
use crate::daemon::domain::Domain;
use crate::gelf::message::{Level, LogEntry};
use crate::nightly::{CaptureSnapshot, Selector, Summary};
use crate::span::types::SpanEntry;
use chrono::{DateTime, Utc};
use logmon_broker_protocol::*;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const MAX_SNAPSHOTS: usize = 8;
const MAX_WRITERS: usize = 2;

fn loss_counters(d: &Domain) -> [u64; 4] {
    let drops = d.metrics.snapshot();
    let spans = d.metrics.trace_ingest_loss();
    [
        drops.gelf_tcp
            + drops.gelf_udp
            + drops.otlp_http_logs
            + drops.otlp_grpc_logs
            + d.metrics.oversize_dropped(),
        spans.dropped,
        spans.malformed,
        spans.shed_batches,
    ]
}

struct Execution {
    start: TestLogStart,
    end: Option<TestLogEnd>,
    received: Instant,
    baseline_drops: Option<[u64; 4]>,
    last_seen: DateTime<Utc>,
    timeout_ms: u64,
    timed_out: bool,
    forced: bool,
    deadline: Option<Instant>,
    summary: Option<Summary>,
    sampled: bool,
    coverage: Value,
    decision: Option<String>,
    reason: Option<&'static str>,
    capture_deadline: Option<Instant>,
    late_logs: u64,
    late_spans: u64,
}

impl Execution {
    fn register_loss(&mut self, d: &Domain) {
        self.coverage["attributable_drops"] = Value::Null;
        self.coverage["domain_loss_during_window"] =
            self.baseline_drops.map_or(Value::Null, |baseline| {
                let now = loss_counters(d);
                json!({
                    "logs_dropped": now[0].saturating_sub(baseline[0]),
                    "spans_dropped": now[1].saturating_sub(baseline[1]),
                    "malformed_spans": now[2].saturating_sub(baseline[2]),
                    "shed_trace_batches": now[3].saturating_sub(baseline[3])
                })
            });
    }
    fn summary_data(&self) -> Value {
        json!({"execution_id":self.start.execution_id,"name":self.start.name,
          "selector":self.start.selector,"started_at":self.start.started_at,
          "ended_at":self.end.as_ref().map(|e|e.ended_at),"verdict":self.end.as_ref().map(|e|e.verdict),
          "elapsed_ms":self.end.as_ref().map(|e|(e.ended_at-self.start.started_at).num_milliseconds()),
          "estimated_ms":self.start.estimated_ms,"test_timeout_ms":self.timeout_ms,
          "case_reason":self.reason,"slow_if":self.start.slow_if,
          "summary_status":if self.sampled{"collected"}else{"sampled_out"},
          "summary":self.summary.as_ref().map(Summary::json),"coverage":self.coverage,
          "closure_incomplete":self.forced || self.timed_out,"rerun_of":self.start.rerun_of})
    }
}

struct Pending {
    snapshot: CaptureSnapshot,
    ready: Instant,
    reason: &'static str,
    order: u64,
    metadata: Value,
    registry: Vec<RegistryFact>,
}

pub(super) struct Run {
    pub config: TestLogBegin,
    pub journal: Arc<Journal>,
    pub last_activity: DateTime<Utc>,
    pub closing: bool,
    close_written: bool,
    entries: HashMap<String, Execution>,
    // Keep ingestion proportional to selector fields and matching executions.
    index: HashMap<String, HashMap<String, Vec<String>>>,
    active: HashSet<String>,
    grace: HashSet<String>,
    pending: HashMap<String, Pending>,
    writing: HashSet<String>,
    dirty_late: HashSet<String>,
    last_late_flush: Instant,
    primary: u64,
    reruns: u64,
    slow: u64,
    order: u64,
    finished_tx: mpsc::Sender<(String, Result<std::path::PathBuf, String>)>,
    finished_rx: mpsc::Receiver<(String, Result<std::path::PathBuf, String>)>,
    provenance: Vec<RegistryFact>,
}

fn sampled(name: &str, percent: f64) -> bool {
    if percent >= 100.0 {
        return true;
    }
    if percent <= 0.0 {
        return false;
    }
    let hash = name.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    (hash as f64) < percent * 0.01 * (u64::MAX as f64)
}

enum Slow {
    Off,
    Estimate(f64),
    Millis(u64),
}
impl Slow {
    fn parse(raw: &str) -> Result<Self, String> {
        let text: String = raw.split_whitespace().collect();
        if text == "off" {
            return Ok(Self::Off);
        }
        let val = text
            .strip_prefix("elapsed>")
            .ok_or("slow_if requires elapsed > duration or elapsed > factor * estimated")?;
        if let Some(m) = val.strip_suffix("*estimated") {
            let m: f64 = m.parse().map_err(|_| "invalid estimate multiplier")?;
            if m.is_finite() && m > 0.0 {
                return Ok(Self::Estimate(m));
            }
            return Err("invalid estimate multiplier".into());
        }
        for (unit, scale) in [("ms", 1), ("s", 1000), ("m", 60000)] {
            if let Some(n) = val.strip_suffix(unit) {
                return n
                    .parse::<u64>()
                    .ok()
                    .and_then(|v| v.checked_mul(scale))
                    .map(Self::Millis)
                    .ok_or("invalid duration".into());
            }
        }
        Err("invalid slow_if duration".into())
    }
    fn matches(self, ms: i64, est: Option<u64>) -> bool {
        match self {
            Self::Off => false,
            Self::Millis(n) => ms as f64 > n as f64,
            Self::Estimate(n) => est.is_some_and(|e| ms as f64 > n * e as f64),
        }
    }
}

impl Run {
    pub fn validate_config(c: &TestLogBegin) -> Result<(), String> {
        if c.test_timeout_ms == Some(0) || c.lifecycle_ack_timeout_ms == Some(0) {
            return Err("timeouts must be positive".into());
        }
        if c.summary_sample_percent
            .is_some_and(|p| !(0.0..=100.0).contains(&p))
        {
            return Err("summary_sample_percent must be between 0 and 100".into());
        }
        if let Some(cond) = &c.slow_if {
            Slow::parse(cond)?;
        }
        match c.slow_pass.as_deref() {
            None | Some("opt-in") => (),
            Some("opt-out") if c.slow_if.is_some() => (),
            _ => return Err("use opt-in, or opt-out with a default slow_if condition".into()),
        }
        let grace = Duration::from_millis(c.grace_period_ms.unwrap_or(10000));
        if Instant::now().checked_add(grace).is_none() {
            return Err("grace period cannot be represented".into());
        }
        Ok(())
    }
    pub fn new(config: TestLogBegin, journal: Journal, provenance: Vec<RegistryFact>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            config,
            journal: Arc::new(journal),
            last_activity: Utc::now(),
            closing: false,
            close_written: false,
            entries: HashMap::new(),
            index: HashMap::new(),
            active: HashSet::new(),
            grace: HashSet::new(),
            pending: HashMap::new(),
            writing: HashSet::new(),
            primary: 0,
            dirty_late: HashSet::new(),
            last_late_flush: Instant::now(),
            reruns: 0,
            slow: 0,
            order: 0,
            finished_tx: tx,
            finished_rx: rx,
            provenance,
        }
    }
    pub fn append(&self, event: &str, mut data: Value) {
        data["event"] = json!(event);
        data["run_id"] = json!(self.config.run_id);
        self.journal.append(data);
    }
    fn grace_deadline(&self) -> Instant {
        Instant::now() + Duration::from_millis(self.config.grace_period_ms.unwrap_or(10000))
    }
    fn register(
        &mut self,
        s: TestLogStart,
        mode: &str,
        baseline_drops: Option<[u64; 4]>,
    ) -> Result<(), String> {
        if s.execution_id.is_empty()
            || s.name.is_empty()
            || s.selector.field.is_empty()
            || s.selector.value.is_empty()
        {
            return Err("missing execution identity, name or selector".into());
        }
        if s.test_timeout_ms == Some(0) {
            return Err("execution timeout must be positive".into());
        }
        if let Some(cond) = &s.slow_if {
            Slow::parse(cond)?;
        }
        let sampled = sampled(&s.name, self.config.summary_sample_percent.unwrap_or(100.));
        let summary = sampled.then(|| {
            Summary::new(
                &s.selector.field,
                &s.selector.value,
                &self.config.collect_distinct,
            )
        });
        let timeout_ms = s
            .test_timeout_ms
            .or(self.config.test_timeout_ms)
            .unwrap_or(600000);
        let id = s.execution_id.clone();
        let key = if s.selector.field == "trace_id" {
            let trace = u128::from_str_radix(&s.selector.value, 16)
                .map_err(|_| "invalid trace_id selector")?;
            format!("{trace:032x}")
        } else {
            s.selector.value.clone()
        };
        self.index
            .entry(s.selector.field.clone())
            .or_default()
            .entry(key)
            .or_default()
            .push(id.clone());
        self.active.insert(id.clone());
        self.entries.insert(
            id,
            Execution {
                last_seen: s.started_at,
                start: s,
                received: Instant::now(),
                baseline_drops,
                timeout_ms,
                summary,
                sampled,
                end: None,
                decision: None,
                reason: None,
                capture_deadline: None,
                timed_out: false,
                forced: false,
                deadline: None,
                coverage: json!({"collection_mode":mode,"coverage_as_of":Utc::now()}),
                late_logs: 0,
                late_spans: 0,
            },
        );
        Ok(())
    }
    pub fn start(&mut self, d: &Domain, s: TestLogStart) -> Result<Value, String> {
        if let Some(old) = self.entries.get(&s.execution_id) {
            if old.start.name != s.name
                || old.start.selector.field != s.selector.field
                || old.start.selector.value != s.selector.value
            {
                return Ok(refused(&s.run_id, "execution_identity_conflict"));
            }
            return Ok(
                json!({"status":"already_applied","run_id":s.run_id,"execution_id":s.execution_id}),
            );
        }
        if self.closing {
            return Ok(refused(&s.run_id, "run_closed"));
        }
        let payload = serde_json::to_value(&s).unwrap();
        let id = s.execution_id.clone();
        self.register(s, "live", Some(loss_counters(d)))?;
        self.append("start-test", payload);
        Ok(json!({"status":"accepted","run_id":self.config.run_id,"execution_id":id}))
    }
    pub fn machine_load(&mut self, domain: &Domain, sample: TestLogMachineLoad) -> Value {
        if self.closing {
            return refused(&sample.run_id, "run_closed");
        }
        self.append("machine-load", serde_json::to_value(&sample).unwrap());
        // Keep machine telemetry subject to the same retention and snapshot
        // budget as every other record. It has no execution selector.
        let mut row = LogEntry::synthetic(Level::Info, "machine-load");
        row.timestamp = sample.ts;
        row.host = sample.host;
        row.seq = domain.pipeline.assign_seq();
        row.additional_fields
            .insert("meta_log".into(), json!("machine-load"));
        row.additional_fields
            .insert("run_id".into(), json!(sample.run_id));
        row.additional_fields
            .insert("sample".into(), json!(sample.fields));
        domain.pipeline.append_to_store(row.clone());
        self.observe_log(&row);
        json!({"status":"accepted","run_id":self.config.run_id})
    }
    pub fn end(
        &mut self,
        d: &Domain,
        e: TestLogEnd,
        replay: bool,
        reg: &[RegistryFact],
    ) -> Result<Value, String> {
        let id = e.execution_id.clone();
        if let Some(old) = self.entries.get(&id).filter(|old| old.end.is_some()) {
            return Ok(
                json!({"status":"already_applied","run_id":e.run_id,"execution_id":id,"case_status":old.decision}),
            );
        }
        if self.closing {
            return Ok(refused(&e.run_id, "run_closed"));
        }
        if e.ended_at < e.started_at {
            return Err("end precedes start".into());
        }
        if let Some(old) = self.entries.get(&id) {
            if old.start.name != e.name
                || old.start.selector.field != e.selector.field
                || old.start.selector.value != e.selector.value
            {
                return Ok(refused(&e.run_id, "execution_identity_conflict"));
            }
        } else {
            self.register(
                TestLogStart {
                    run_id: e.run_id.clone(),
                    execution_id: id.clone(),
                    name: e.name.clone(),
                    selector: e.selector.clone(),
                    started_at: e.started_at,
                    estimated_ms: None,
                    test_timeout_ms: None,
                    slow_if: None,
                    rerun_of: e.rerun_of.clone(),
                },
                "ring_backfill",
                None,
            )?;
            let ex = self.entries.get_mut(&id).unwrap();
            if let Some(summary) = &mut ex.summary {
                let mut coverage = summary.backfill(d, e.started_at, e.ended_at);
                coverage["collection_mode"] = json!("ring_backfill");
                coverage["coverage_as_of"] = json!(Utc::now());
                ex.coverage = coverage;
            }
        }
        let deadline = self.grace_deadline();
        let ex = self.entries.get_mut(&id).unwrap();
        let elapsed = (e.ended_at - ex.start.started_at).num_milliseconds();
        let condition = ex.start.slow_if.as_deref().or_else(|| {
            if self.config.slow_pass.as_deref() == Some("opt-out") {
                self.config.slow_if.as_deref()
            } else {
                None
            }
        });
        let reason = if e.rerun_of.is_some() {
            Some("rerun")
        } else if e.verdict != TestVerdict::Ok {
            Some("failure")
        } else if condition
            .and_then(|c| Slow::parse(c).ok())
            .is_some_and(|s| s.matches(elapsed, ex.start.estimated_ms))
        {
            Some("slow_pass")
        } else {
            None
        };
        ex.register_loss(d);
        ex.end = Some(e.clone());
        ex.deadline = Some(deadline);
        self.active.remove(&id);
        self.grace.insert(id.clone());
        self.provenance = reg.to_vec();
        if self.entries[&id].decision.is_none() {
            if let Some(reason) = reason {
                self.admit(d, &id, reason, e.ended_at, replay, reg);
            }
        }
        let ex = &self.entries[&id];
        let mut data = ex.summary_data();
        data["fields"] = json!(e.fields);
        data["case_status"] = json!(ex.decision);
        data["snapshot_delayed"] = json!(replay);
        if let Some(value) = e.fields.get("message") {
            data["message"] = value.clone();
        }
        if let Some(value) = e.fields.get("failed_test") {
            data["failed_test"] = value.clone();
        }
        self.append("end-test", data);
        // The writer and any admitted case now own the assertion/output. The
        // overlap index only needs identity, timestamps and verdict.
        self.entries
            .get_mut(&id)
            .unwrap()
            .end
            .as_mut()
            .unwrap()
            .fields
            .clear();
        Ok(
            json!({"status":"accepted","run_id":e.run_id,"execution_id":id,"case_status":self.entries[&id].decision}),
        )
    }

    fn admit(
        &mut self,
        d: &Domain,
        id: &str,
        reason: &'static str,
        end: DateTime<Utc>,
        delayed: bool,
        reg: &[RegistryFact],
    ) {
        self.entries.get_mut(id).unwrap().reason = Some(reason);
        let is_slow = reason == "slow_pass";
        let is_rerun = self.entries[id].start.rerun_of.is_some();
        let skip = if is_rerun {
            self.reruns += 1;
            (self.reruns > self.config.max_rerun_cases.unwrap_or(10))
                .then_some("skipped_rerun_limit")
        } else if is_slow {
            (self.slow >= self.config.max_slow_pass_cases.unwrap_or(20))
                .then_some("skipped_slow_limit")
        } else {
            self.primary += 1;
            (self.primary > self.config.max_failures.unwrap_or(500))
                .then_some("skipped_failure_limit")
        };
        if let Some(skip) = skip {
            self.entries.get_mut(id).unwrap().decision = Some(skip.into());
            return;
        }
        if self.pending.len() + self.writing.len() >= MAX_SNAPSHOTS {
            let displace = (!is_slow)
                .then(|| {
                    self.pending
                        .iter()
                        .filter(|(_, p)| p.reason == "slow_pass")
                        .max_by_key(|(_, p)| p.order)
                        .map(|(id, _)| id.clone())
                })
                .flatten();
            if let Some(old) = displace {
                self.pending.remove(&old);
                self.append(
                    "case-skipped",
                    json!({"execution_id":old,"case_status":"evicted_for_priority"}),
                );
            } else {
                self.entries.get_mut(id).unwrap().decision = Some("skipped_queue_full".into());
                return;
            }
        }
        let ex = &self.entries[id];
        let start = ex.start.started_at;
        let selector = Selector::new(&ex.start.selector.field, &ex.start.selector.value);
        let snapshot = CaptureSnapshot::new(d, selector, start, end);
        let peers: Vec<_> = self
            .entries
            .iter()
            .filter(|(peer_id, peer)| {
                peer_id.as_str() != id
                    && peer.start.started_at <= end
                    && peer.end.as_ref().is_none_or(|e| e.ended_at >= start)
            })
            .map(|(peer_id, peer)| {
                json!({
                    "execution_id": peer_id,
                    "name": peer.start.name,
                    "selector": peer.start.selector,
                    "started_at": peer.start.started_at,
                    "ended_at": peer.end.as_ref().map(|e|e.ended_at),
                    "active_at_capture": peer.end.is_none() && !peer.forced
                })
            })
            .collect();
        let metadata = json!({
            "run_id": self.config.run_id,
            "domain": d.id().as_str(),
            "execution_id": id,
            "name": ex.start.name,
            "reason": reason,
            "selector": ex.start.selector,
            "started_at": start,
            "ended_at": ex.end.as_ref().map(|e|e.ended_at),
            "verdict": ex.end.as_ref().map(|e|e.verdict),
            "end": ex.end,
            "peers": peers,
            "estimated_ms": ex.start.estimated_ms,
            "snapshot_delayed": delayed,
            "captured_at": Utc::now(),
        });
        self.order += 1;
        if is_slow {
            self.slow += 1;
        }
        let ready = self.grace_deadline();
        self.entries.get_mut(id).unwrap().capture_deadline = Some(ready);
        self.pending.insert(
            id.into(),
            Pending {
                snapshot,
                ready,
                reason,
                order: self.order,
                metadata,
                registry: reg.to_vec(),
            },
        );
        self.entries.get_mut(id).unwrap().decision = Some("pending".into());
    }

    pub fn observe_log(&mut self, log: &LogEntry) {
        let now = Instant::now();
        for p in self.pending.values_mut().filter(|p| now < p.ready) {
            p.snapshot.observe_log(log);
        }
        let ids: Vec<_> = self
            .index
            .iter()
            .filter_map(|(field, values)| {
                let value = Selector::log_value(field, log)?;
                values.get(value.as_ref())
            })
            .flatten()
            .cloned()
            .collect();
        for id in ids {
            let ex = self.entries.get_mut(&id).unwrap();
            let diagnostic_deadline = ex.capture_deadline.or(ex.deadline);
            if diagnostic_deadline.is_some_and(|t| now >= t) {
                ex.late_logs += 1;
                self.dirty_late.insert(id.clone());
                let past_grace_ms = now
                    .saturating_duration_since(diagnostic_deadline.unwrap())
                    .as_millis() as u64;
                if ex.late_logs + ex.late_spans == 1 {
                    self.append("late-telemetry",json!({"execution_id":id,"kind":"log","timestamp":log.timestamp,"arrived_at":Utc::now(),"past_grace_ms":past_grace_ms}));
                }
            }
            let ex = self.entries.get_mut(&id).unwrap();
            if ex.deadline.is_none_or(|t| now < t) {
                ex.last_seen = ex.last_seen.max(log.timestamp);
                if let Some(summary) = &mut ex.summary {
                    summary.observe_log(log);
                }
            }
        }
    }
    pub fn observe_span(&mut self, span: &SpanEntry) {
        let now = Instant::now();
        for p in self.pending.values_mut().filter(|p| now < p.ready) {
            p.snapshot.observe_span(span);
        }
        let ids: Vec<_> = self
            .index
            .iter()
            .filter_map(|(field, values)| {
                let value = Selector::span_value(field, span)?;
                values.get(value.as_ref())
            })
            .flatten()
            .cloned()
            .collect();
        for id in ids {
            let ex = self.entries.get_mut(&id).unwrap();
            let diagnostic_deadline = ex.capture_deadline.or(ex.deadline);
            if diagnostic_deadline.is_some_and(|t| now >= t) {
                ex.late_spans += 1;
                self.dirty_late.insert(id.clone());
                let past_grace_ms = now
                    .saturating_duration_since(diagnostic_deadline.unwrap())
                    .as_millis() as u64;
                if ex.late_logs + ex.late_spans == 1 {
                    self.append("late-telemetry",json!({"execution_id":id,"kind":"span","started_at":span.start_time,"ended_at":span.end_time,"arrived_at":Utc::now(),"past_grace_ms":past_grace_ms}));
                }
            }
            let ex = self.entries.get_mut(&id).unwrap();
            if ex.deadline.is_none_or(|t| now < t) {
                ex.last_seen = ex.last_seen.max(span.end_time);
                if let Some(summary) = &mut ex.summary {
                    summary.observe_span(span);
                }
            }
        }
    }

    fn force(
        &mut self,
        d: &Domain,
        id: &str,
        reason: &'static str,
        reg: &[RegistryFact],
        closing: bool,
    ) {
        let deadline = self.grace_deadline();
        let ex = self.entries.get_mut(id).unwrap();
        ex.register_loss(d);
        ex.forced = closing;
        if !closing {
            ex.timed_out = true;
        }
        self.active.remove(id);
        if closing {
            ex.deadline = Some(deadline);
            self.grace.insert(id.into());
        }
        if ex.decision.is_none() {
            let end = if closing { ex.last_seen } else { Utc::now() };
            self.admit(d, id, reason, end, false, reg);
        }
        let ex = &self.entries[id];
        let mut data = ex.summary_data();
        data["reason"] = json!(reason);
        data["last_observed_at"] = json!(ex.last_seen);
        data[if closing { "closed_at" } else { "captured_at" }] = json!(Utc::now());
        data["case_status"] = json!(ex.decision);
        self.append("incomplete", data);
    }

    pub fn close(&mut self, d: &Domain, seal: bool, reg: &[RegistryFact]) {
        if self.closing {
            return;
        }
        self.closing = true;
        let mut open: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, e)| e.end.is_none())
            .map(|(id, _)| id.clone())
            .collect();
        open.sort_by(|a, b| {
            self.entries[a]
                .start
                .started_at
                .cmp(&self.entries[b].start.started_at)
                .then_with(|| a.cmp(b))
        });
        let reason = if seal {
            "abandoned_run_sealed"
        } else {
            "run_ended_without_end_test"
        };
        for id in open {
            self.force(d, &id, reason, reg, true);
        }
        self.append(
            if seal {
                "seal-test-log"
            } else {
                "end-test-log"
            },
            json!({"at":Utc::now(),"incomplete":seal || self.entries.values().any(|e|e.forced)}),
        );
    }

    /// Sleep until the next deadline; an RPC wakes this thread when it changes
    /// the work list. Only active file jobs need a short completion poll.
    pub fn next_wake(&self, now: Instant) -> Duration {
        let mut delay = if self.writing.is_empty() {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(10)
        };
        for id in &self.active {
            let e = &self.entries[id];
            delay = delay.min(
                Duration::from_millis(e.timeout_ms)
                    .saturating_sub(now.saturating_duration_since(e.received)),
            );
        }
        for id in &self.grace {
            if let Some(deadline) = self.entries[id].deadline {
                delay = delay.min(deadline.saturating_duration_since(now));
            }
        }
        if self.writing.len() < MAX_WRITERS {
            for capture in self.pending.values() {
                delay = delay.min(capture.ready.saturating_duration_since(now));
            }
        }
        if !self.dirty_late.is_empty() {
            delay = delay.min(
                Duration::from_secs(1)
                    .saturating_sub(now.saturating_duration_since(self.last_late_flush)),
            );
        }
        delay
    }

    pub fn flush_late_counts(&mut self, now: Instant) {
        for id in std::mem::take(&mut self.dirty_late) {
            let e = &self.entries[&id];
            self.append("late-telemetry-counts",json!({"execution_id":id,"logs":e.late_logs,"spans":e.late_spans,"as_of":Utc::now()}));
        }
        self.last_late_flush = now;
    }

    pub fn tick(&mut self, d: &Arc<Domain>, now: Instant) {
        if now.saturating_duration_since(self.last_late_flush) >= Duration::from_secs(1)
            && !self.dirty_late.is_empty()
        {
            self.flush_late_counts(now);
        }
        let timeouts: Vec<_> = self
            .active
            .iter()
            .filter(|id| {
                let ex = &self.entries[*id];
                now.saturating_duration_since(ex.received) >= Duration::from_millis(ex.timeout_ms)
            })
            .cloned()
            .collect();
        if !timeouts.is_empty() {
            let reg = self.provenance.clone();
            for id in timeouts {
                self.force(d, &id, "end_test_missing_at_deadline", &reg, false);
            }
        }
        let ready: Vec<_> = self
            .grace
            .iter()
            .filter(|id| self.entries[*id].deadline.is_some_and(|t| t <= now))
            .cloned()
            .collect();
        for id in ready {
            let ex = self.entries.get_mut(&id).unwrap();
            ex.register_loss(d);
            ex.coverage["finalized_at"] = json!(Utc::now());
            let data = ex.summary_data();
            ex.summary = None;
            self.grace.remove(&id);
            self.append("summary-final", data);
        }
        while let Ok((id, result)) = self.finished_rx.try_recv() {
            self.writing.remove(&id);
            match result{
                Ok(path)=>{
                    match path.strip_prefix(self.journal.path.parent().unwrap()) {
                        Ok(relative)=>self.append("case-ready",json!({"execution_id":id,"case_status":"ready","case_path":relative})),
                        Err(_)=>self.append("case-error",json!({"execution_id":id,"case_status":"error","message":"case outside run directory"})),
                    }
                }
                Err(error)=>self.append("case-error",json!({"execution_id":id,"case_status":"error","message":error})),
            }
        }
        while self.writing.len() < MAX_WRITERS {
            let candidate = self
                .pending
                .iter()
                .filter(|(_, p)| p.ready <= now)
                .min_by_key(|(_, p)| (p.reason == "slow_pass", p.order))
                .map(|(id, _)| id.clone());
            let Some(id) = candidate else { break };
            let p = self.pending.remove(&id).unwrap();
            let tx = self.finished_tx.clone();
            let key = id.clone();
            let dir = self.journal.path.parent().unwrap().join("cases");
            match std::thread::Builder::new()
                .name("test-case-writer".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        p.snapshot.write(&dir, &p.metadata, p.registry)
                    }))
                    .unwrap_or_else(|_| Err("case writer panicked".into()));
                    let _ = tx.send((key, result));
                }) {
                Ok(_) => {
                    self.writing.insert(id);
                }
                Err(e) => self.append(
                    "case-error",
                    json!({"execution_id":id,"case_status":"error","message":e.to_string()}),
                ),
            }
        }
        if self.closing
            && !self.close_written
            && self.grace.is_empty()
            && self.pending.is_empty()
            && self.writing.is_empty()
        {
            self.close_written = true;
            self.append(
                "run-settled",
                json!({"at":Utc::now(),"executions":self.entries.len()}),
            );
        }
    }

    pub fn close_result(&self) -> Value {
        let mut out = self.journal.status();
        out["status"] = json!("accepted");
        out["run_id"] = json!(self.config.run_id);
        out["finalization"] = json!(if self.close_written {
            "settled"
        } else {
            "pending"
        });
        out["pending_summaries"] = json!(self.grace.len() + self.active.len());
        let now = Instant::now();
        let mut pending = Vec::with_capacity(self.pending.len() + self.writing.len());
        for (id, capture) in &self.pending {
            pending.push(json!({
                "execution_id": id,
                "name": self.entries[id].start.name,
                "state": if capture.ready > now {"in_grace"} else {"queued"},
            }));
        }
        for id in &self.writing {
            pending.push(json!({
                "execution_id": id,
                "name": self.entries[id].start.name,
                "state":"writing",
            }));
        }
        out["pending_cases"] = json!(pending);
        out
    }
}
