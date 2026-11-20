use super::{
    quota::{Quotas, Record, TailWriter, FAULT},
    refused,
    writer::Journal,
};
use crate::cases::document::RegistryFact;
use crate::daemon::domain::Domain;
use crate::gelf::message::{Level, LogEntry};
use crate::nightly::{CaptureSize, CaptureSnapshot, Selector, Summary};
use crate::span::types::SpanEntry;
use chrono::{DateTime, Utc};
use logmon_broker_protocol::*;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{mpsc, Arc, Weak};
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
    end_fields_hash: u64,
    member_runtime_ms: Option<u64>,
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
    case_size: Option<CaptureSize>,
    case_detail: Option<&'static str>,
    reason: Option<&'static str>,
    capture_deadline: Option<Instant>,
    late_logs: u64,
    late_spans: u64,
    quota: usize,
    telemetry_end: Option<Value>,
}

impl Execution {
    fn matches_identity(
        &self,
        name: &str,
        selector: &TestLogSelector,
        start: DateTime<Utc>,
        rerun: Option<&str>,
    ) -> bool {
        name == self.start.name
            && selector.field == self.start.selector.field
            && selector.value == self.start.selector.value
            && start == self.start.started_at
            && rerun == self.start.rerun_of.as_deref()
    }

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
          "member_runtime_ms": self.member_runtime_ms,"deadline_crossed":self.timed_out,"supersedes_incomplete":self.timed_out && self.end.is_some(),"estimated_ms":self.start.estimated_ms,"test_timeout_ms":self.timeout_ms,
          "case_reason":self.reason,"case_size":self.case_size.map(CaptureSize::as_str),
          "case_detail":self.case_detail,"slow_if":self.start.slow_if,
          "summary_status":if self.sampled{"collected"}else{"sampled_out"},
          "summary":self.summary.as_ref().map(Summary::json),"coverage":self.coverage,
          "closure_incomplete":self.forced || (self.timed_out && self.end.is_none()),"rerun_of":self.start.rerun_of})
    }
}

struct Pending {
    snapshot: CaptureSnapshot,
    ready: Instant,
    reason: &'static str,
    order: u64,
    metadata: Value,
    registry: Vec<RegistryFact>,
    group_until: Option<Instant>,
    members: Vec<(String, Instant, Value, CaptureSnapshot)>,
}

impl Pending {
    fn ready_at(&self, closing: bool) -> Instant {
        let grace = self.members.iter().fold(self.ready, |at, m| at.max(m.1));
        if closing {
            grace
        } else {
            self.group_until.unwrap_or(grace).max(grace)
        }
    }
    fn write(mut self, dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
        if self.group_until.is_some() {
            let mut first = self.metadata.clone();
            first["coverage"] = self.snapshot.coverage();
            let mut group = vec![first];
            let mut peers = self.metadata["peers"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for (_, _, mut meta, snap) in self.members {
                meta["coverage"] = snap.coverage();
                for peer in meta["peers"].as_array().into_iter().flatten() {
                    if !peers
                        .iter()
                        .any(|p| p["execution_id"] == peer["execution_id"])
                    {
                        peers.push(peer.clone());
                    }
                }
                self.snapshot.merge(snap);
                group.push(meta);
            }
            peers.retain(|peer| {
                !group
                    .iter()
                    .any(|m| m["execution_id"] == peer["execution_id"])
            });
            self.metadata["peers"] = json!(peers);
            self.metadata["deadline_members"] = json!(group);
        }
        self.snapshot.write(dir, &self.metadata, self.registry)
    }
}

pub(super) struct Run {
    pub(super) progress: super::progress::Progress,
    pub config: TestLogBegin,
    pub journal: Arc<Journal>,
    samples: MachineSeries<Arc<LogEntry>>,
    sample_bytes: usize,
    began_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub closing: bool,
    close_written: bool,
    entries: HashMap<String, Execution>,
    // Keep ingestion proportional to selector fields and matching executions.
    index: HashMap<String, HashMap<String, usize>>,
    quotas: Quotas,
    tail_writer: TailWriter,
    domain: Weak<Domain>,
    active: HashSet<String>,
    grace: HashSet<String>,
    pending: HashMap<String, Pending>,
    writing: HashSet<String>,
    writing_members: HashMap<String, Vec<String>>,
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

#[derive(Clone, Copy)]
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
    #[cfg(test)]
    pub(super) fn take_pending_for_test(&mut self, id: &str, writing: bool) {
        self.pending.remove(id).expect("pending case for test");
        if writing {
            assert!(self.writing.insert(id.into()));
        }
    }

    #[cfg(test)]
    pub(super) fn finish_writer_for_test(&self, id: &str) {
        self.finished_tx
            .send((id.into(), Err("test writer finished".into())))
            .unwrap();
    }

    fn matching_groups<'a>(&'a self, record: Record<'a>) -> impl Iterator<Item = usize> + 'a {
        self.index.iter().filter_map(move |(field, values)| {
            let value = match record {
                Record::Log(log) => Selector::log_value(field, log),
                Record::Span(span) => Selector::span_value(field, span),
            }?;
            values.get(value.as_ref()).copied()
        })
    }

    pub fn admit_record(&mut self, record: Record<'_>) -> (bool, bool) {
        let groups = if self.active.is_empty() {
            Vec::new()
        } else {
            self.matching_groups(record)
                .filter(|&id| self.quotas.groups[id].live > 0)
                .collect()
        };
        let (accepted, mut crossed) = self.quotas.admit(record, &groups);
        crossed.sort_unstable();
        for &group in &crossed {
            self.capture_crossing(group);
        }
        (accepted, !crossed.is_empty())
    }

    fn telemetry_fault(&self, id: &str) -> Value {
        let ex = &self.entries[id];
        let group = &self.quotas.groups[ex.quota];
        let Some(crossing) = group.crossing else {
            return Value::Null;
        };
        let peers = group
            .executions
            .iter()
            .filter(|peer| peer.as_str() != id)
            .map(|peer| json!({"execution_id":peer,"name":self.entries[peer].start.name}))
            .collect();
        json!({"fault":FAULT,"telemetry_limit":crossing.details(peers)})
    }

    fn append_telemetry_fault(&self, id: &str) {
        let mut data = self.telemetry_fault(id);
        data["execution_id"] = json!(id);
        data["name"] = json!(self.entries[id].start.name);
        data["case_status"] = json!(self.entries[id].decision);
        data["case_size"] = json!(self.entries[id].case_size.map(CaptureSize::as_str));
        self.append("telemetry-fault", data);
    }

    fn capture_crossing(&mut self, group: usize) {
        let members = self.quotas.groups[group].executions.clone();
        let Some(owner) = members.iter().find(|id| self.active.contains(*id)).cloned() else {
            return;
        };
        let Some(domain) = self.domain.upgrade() else {
            return;
        };
        self.quotas.groups[group].case_owner = Some(owner.clone());
        let registry = self.provenance.clone();
        self.admit(&domain, &owner, FAULT, Utc::now(), false, &registry);
        for id in members {
            self.join_crossing_case(&id);
        }
    }

    fn join_crossing_case(&mut self, id: &str) {
        let group = &self.quotas.groups[self.entries[id].quota];
        let Some(owner) = &group.case_owner else {
            return;
        };
        let size = self.entries[owner].case_size;
        let detail = self.entries[owner].case_detail;
        let ex = self.entries.get_mut(id).unwrap();
        if ex.decision.is_none() {
            ex.reason = Some(FAULT);
            ex.case_size = size;
            ex.case_detail = detail;
            ex.decision = Some("pending".into());
        }
        self.append_telemetry_fault(id);
        if self.entries[id].reason == Some(FAULT) && id != owner {
            if let Some(path) = &group.case_path {
                self.append(
                    "case-ready",
                    json!({"execution_id":id,"case_status":"ready",
                    "case_path":path,"case_size":size.map(CaptureSize::as_str),"case_detail":detail}),
                );
            } else if let Some(error) = &group.case_error {
                self.append(
                    "case-error",
                    json!({"execution_id":id,"case_status":"error","message":error}),
                );
            }
        }
    }

    fn finish_telemetry(&mut self, id: &str) {
        if !self.active.remove(id) {
            return;
        }
        let quota = self.entries[id].quota;
        if self.quotas.groups[quota].crossing.is_some() {
            let dir = self.journal.path.parent().expect("absolute journal");
            let (mut data, job) = self.quotas.save(quota, id, dir);
            if let Some(job) = job {
                if let Err(error) = self.tail_writer.send(job) {
                    data.as_object_mut().unwrap().remove("telemetry_tail_path");
                    data["telemetry_tail_error"] = json!(error);
                }
            }
            self.entries.get_mut(id).unwrap().telemetry_end = Some(data);
        }
        self.quotas.finish(quota);
    }

    fn add_telemetry(&self, id: &str, data: &mut Value) {
        if let Some(fault) = self.telemetry_fault(id).as_object() {
            let result = data.as_object_mut().expect("execution row");
            result.extend(fault.clone());
            if let Some(info) = &self.entries[id].telemetry_end {
                result.extend(info.as_object().expect("end telemetry").clone());
            }
        }
    }

    fn is_step(&self, id: &str) -> bool {
        self.entries[id].start.name.starts_with("step:")
    }

    fn snapshot_slot_available(&self, step: bool) -> bool {
        let mut total = 0;
        let mut tests = 0;
        // A deadline group reserves one slot from admission through writing.
        for id in self.pending.keys().chain(&self.writing) {
            if self.entries[id].case_size != Some(CaptureSize::Bare) {
                total += 1;
                tests += usize::from(!self.is_step(id));
            }
        }
        total < MAX_SNAPSHOTS && (step || tests < MAX_SNAPSHOTS - 1)
    }

    fn writer_available(&self, size: CaptureSize) -> bool {
        let bare = |id: &String| self.entries[id].case_size == Some(CaptureSize::Bare);
        if size == CaptureSize::Bare {
            !self.writing.iter().any(bare)
        } else {
            self.writing.iter().filter(|id| !bare(id)).count() < MAX_WRITERS
        }
    }

    pub(super) fn update_ring_protection(&self, domain: &Domain) {
        let since = self
            .active
            .iter()
            .chain(&self.grace)
            .filter_map(|id| self.entries.get(id))
            .filter(|e| e.decision.as_deref() != Some("pending"))
            .filter(|e| e.deadline.is_none_or(|at| at > Instant::now()) || e.end.is_none())
            .map(|e| e.start.started_at)
            .min();
        domain.pipeline.store.protect_since(since);
        domain.span_store.protect_since(since);
    }

    pub fn validate_config(c: &TestLogBegin) -> Result<(), String> {
        if let Some(zone) = &c.display_timezone {
            zone.parse::<chrono_tz::Tz>()
                .map_err(|_| format!("invalid IANA time zone {zone}"))?;
        }
        if c.test_timeout_ms == Some(0) || c.lifecycle_ack_timeout_ms == Some(0) {
            return Err("timeouts must be positive".into());
        }
        if c.summary_sample_percent
            .is_some_and(|p| !(0.0..=100.0).contains(&p))
        {
            return Err("summary_sample_percent must be between 0 and 100".into());
        }
        if c.execution_limit_percent
            .is_some_and(|p| !(p > 0. && p <= 100.))
        {
            return Err("execution_limit_percent must be greater than 0 and at most 100".into());
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
        if Instant::now().checked_add(grace).is_none()
            || chrono::Duration::from_std(grace)
                .ok()
                .and_then(|g| Utc::now().checked_add_signed(g))
                .is_none()
        {
            return Err("grace period cannot be represented".into());
        }
        Ok(())
    }
    pub fn new(
        domain: &Arc<Domain>,
        config: TestLogBegin,
        journal: Journal,
        provenance: Vec<RegistryFact>,
        history: Arc<super::progress::History>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let began_at = Utc::now();
        let journal = Arc::new(journal);
        let quotas = Quotas::new(
            [
                domain.pipeline.store.ring_info().0,
                domain.span_store.ring_info().0,
            ]
            .map(|ring| ring.maximum.unwrap_or(ring.current)),
            config.execution_limit_percent.unwrap_or(25.),
        );
        Self {
            progress: super::progress::Progress::new(&config, began_at, journal.clone(), history),
            config,
            journal,
            began_at,
            last_activity: began_at,
            closing: false,
            close_written: false,
            entries: HashMap::new(),
            index: HashMap::new(),
            quotas,
            domain: Arc::downgrade(domain),
            tail_writer: TailWriter::default(),
            active: HashSet::new(),
            grace: HashSet::new(),
            pending: HashMap::new(),
            writing: HashSet::new(),
            writing_members: HashMap::new(),
            samples: MachineSeries::default(),
            sample_bytes: 0,
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
        self.progress.apply(event, &mut data);
        data["event"] = json!(event);
        data["run_id"] = json!(self.config.run_id);
        self.journal.append(data);
    }
    fn grace_window(&self) -> (Instant, DateTime<Utc>, DateTime<Utc>) {
        let grace = Duration::from_millis(self.config.grace_period_ms.unwrap_or(10000));
        let received = Utc::now();
        let deadline = Instant::now() + grace;
        let wall_cutoff =
            received + chrono::Duration::from_std(grace).expect("validated grace duration");
        (deadline, received, wall_cutoff)
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
        let sampled = s.rerun_of.is_some()
            || sampled(&s.name, self.config.summary_sample_percent.unwrap_or(100.));
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
        let quota = *self
            .index
            .entry(s.selector.field.clone())
            .or_default()
            .entry(key)
            .or_insert(self.quotas.groups.len());
        self.quotas.register(quota, id.clone());
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
                end_fields_hash: 0,
                member_runtime_ms: None,
                decision: None,
                case_size: None,
                case_detail: None,
                reason: None,
                capture_deadline: None,
                timed_out: false,
                forced: false,
                deadline: None,
                coverage: json!({"collection_mode":mode,"coverage_as_of":Utc::now()}),
                late_logs: 0,
                late_spans: 0,
                quota,
                telemetry_end: None,
            },
        );
        Ok(())
    }
    pub fn start(&mut self, d: &Domain, s: TestLogStart) -> Result<Value, String> {
        if let Some(old) = self.entries.get(&s.execution_id) {
            if !old.matches_identity(&s.name, &s.selector, s.started_at, s.rerun_of.as_deref()) {
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
        if self.quotas.groups[self.entries[&id].quota]
            .crossing
            .is_some()
        {
            self.join_crossing_case(&id);
        }
        Ok(json!({"status":"accepted","run_id":self.config.run_id,"execution_id":id}))
    }
    pub fn machine_load(&mut self, domain: &Domain, sample: TestLogMachineLoad) -> Value {
        if self.closing {
            return refused(&sample.run_id, "run_closed");
        }
        // Independent, bounded retention. Charge a minimum per row for index overhead.
        let cost = serde_json::to_vec(&sample).unwrap().len() + 512;
        if cost > (8 << 20) - self.sample_bytes {
            return refused(&sample.run_id, "machine_sample_capacity");
        }
        let mut data = serde_json::to_value(&sample).unwrap();
        self.progress.apply("machine-load", &mut data);
        data["event"] = json!("machine-load");
        if !self.journal.append(data) {
            return refused(&sample.run_id, "journal_queue_overflow");
        }
        self.sample_bytes += cost;
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
        row.additional_fields
            .insert("kind".into(), json!(sample.kind));
        let host = row.host.clone();
        self.samples
            .insert(&host, &sample.kind, sample.ts, Arc::new(row));
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
        let (deadline, received_at, cutoff) = self.grace_window();
        // The archive and journal own the potentially large event body after
        // acknowledgement. A digest lets us detect changed replay evidence.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        e.fields.hash(&mut hasher);
        let fields_hash = hasher.finish();
        if let Some(old) = self.entries.get(&id) {
            if !old.matches_identity(&e.name, &e.selector, e.started_at, e.rerun_of.as_deref()) {
                return Ok(refused(&e.run_id, "execution_identity_conflict"));
            }
            if let Some(accepted) = &old.end {
                if accepted.ended_at != e.ended_at
                    || accepted.verdict != e.verdict
                    || old.end_fields_hash != fields_hash
                {
                    return Ok(refused(&e.run_id, "execution_outcome_conflict"));
                }
                let mut ack = json!({"status":"already_applied","run_id":e.run_id,"execution_id":id,"case_status":old.decision,
                    "case_size":old.case_size.map(CaptureSize::as_str),"case_detail":old.case_detail});
                self.add_telemetry(&id, &mut ack);
                return Ok(ack);
            }
        }
        if self.closing {
            return Ok(refused(&e.run_id, "run_closed"));
        }
        if e.ended_at < e.started_at {
            return Err("end precedes start".into());
        }
        if !self.entries.contains_key(&id) {
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
            if self.quotas.groups[self.entries[&id].quota]
                .crossing
                .is_some()
            {
                self.join_crossing_case(&id);
            }
        }
        let ex = self.entries.get_mut(&id).unwrap();
        let promote = !ex.sampled && e.verdict != TestVerdict::Ok;
        if promote {
            ex.summary = Some(Summary::new(
                &e.selector.field,
                &e.selector.value,
                &self.config.collect_distinct,
            ));
            ex.sampled = true;
        }
        if promote || ex.coverage["collection_mode"] == "ring_backfill" {
            ex.coverage = match &mut ex.summary {
                Some(summary) => summary.backfill(d, e.started_at, e.ended_at),
                None => Summary::backfill_coverage(d, e.started_at, e.ended_at),
            };
            ex.coverage["collection_mode"] = json!("ring_backfill");
            ex.coverage["snapshot_at"] = ex.coverage["coverage_as_of"].clone();
        }
        let ex = self.entries.get_mut(&id).unwrap();
        if ex.coverage["collection_mode"] == "live" {
            ex.coverage["snapshot_at"] = json!(received_at);
            ex.coverage["coverage_as_of"] = json!(received_at);
        }
        ex.coverage["grace_cutoff"] = json!(cutoff);
        ex.member_runtime_ms = e.fields.get("member_runtime_ms").and_then(Value::as_u64);
        let elapsed = (e.ended_at - ex.start.started_at).num_milliseconds();
        let condition = ex.start.slow_if.as_deref().or_else(|| {
            if self.config.slow_pass.as_deref() == Some("opt-out") {
                self.config.slow_if.as_deref()
            } else {
                None
            }
        });
        let reason = if e.verdict != TestVerdict::Ok {
            Some("failure")
        } else if e.rerun_of.is_some() {
            Some("rerun")
        } else if condition
            .and_then(|c| Slow::parse(c).ok())
            .is_some_and(|s| {
                s.matches(
                    match s {
                        Slow::Estimate(_) => ex
                            .member_runtime_ms
                            .map_or(elapsed, |ms| i64::try_from(ms).unwrap_or(i64::MAX)),
                        _ => elapsed,
                    },
                    ex.start.estimated_ms,
                )
            })
        {
            Some("slow_pass")
        } else {
            None
        };
        ex.register_loss(d);
        ex.end_fields_hash = fields_hash;
        ex.end = Some(e.clone());
        ex.deadline = Some(deadline);
        self.finish_telemetry(&id);
        self.grace.insert(id.clone());
        self.provenance = reg.to_vec();
        if self.entries[&id].decision.is_none() {
            if let Some(reason) = reason {
                self.entries.get_mut(&id).unwrap().capture_deadline = Some(deadline);
                self.admit(d, &id, reason, e.ended_at, replay, reg);
            }
        }
        let ex = &self.entries[&id];
        let mut data = ex.summary_data();
        self.add_telemetry(&id, &mut data);
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
        let mut ack = json!({"status":"accepted","run_id":e.run_id,"execution_id":id,"case_status":self.entries[&id].decision,
            "case_size":self.entries[&id].case_size.map(CaptureSize::as_str),"case_detail":self.entries[&id].case_detail});
        self.add_telemetry(&id, &mut ack);
        Ok(ack)
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
        let is_rerun = reason == "rerun";
        let optional = is_slow || is_rerun;
        let step = self.is_step(id);
        let mut size = CaptureSize::Full;
        let skip = if reason == FAULT {
            size = CaptureSize::Minimal;
            None
        } else if is_rerun {
            // The cap counts successful rerun attempts requesting capture,
            // including those that find no capture room.
            self.reruns += 1;
            (self.reruns > self.config.max_rerun_cases.unwrap_or(10))
                .then_some("skipped_rerun_limit")
        } else if is_slow {
            (self.slow >= self.config.max_slow_pass_cases.unwrap_or(20))
                .then_some("skipped_slow_limit")
        } else if !step {
            self.primary += 1;
            if self.primary > super::full_failure_limit(&self.config) {
                size = CaptureSize::Minimal;
            }
            None
        } else {
            None
        };
        if let Some(skip) = skip {
            self.entries.get_mut(id).unwrap().decision = Some(skip.into());
            return;
        }
        let now = Instant::now();
        let group = if size == CaptureSize::Full && reason == "end_test_missing_at_deadline" {
            self.pending
                .iter()
                .find(|(key, p)| {
                    self.is_step(key) == step
                        && p.snapshot.size == size
                        && p.group_until.is_some_and(|cutoff| now < cutoff)
                })
                .map(|(key, _)| key.clone())
        } else {
            None
        };
        if group.is_none() && !self.snapshot_slot_available(step) {
            let displace = (!is_slow)
                .then(|| {
                    self.pending
                        .iter()
                        .filter(|(key, p)| {
                            p.reason == "slow_pass"
                                && p.snapshot.size != CaptureSize::Bare
                                && (step || !self.is_step(key))
                        })
                        .max_by_key(|(_, p)| p.order)
                        .map(|(id, _)| id.clone())
                })
                .flatten();
            if let Some(old) = displace {
                self.pending.remove(&old);
                let evicted = self.entries.get_mut(&old).unwrap();
                evicted.decision = Some("evicted_for_priority".into());
                evicted.case_size = None;
                evicted.case_detail = None;
                self.append(
                    "case-skipped",
                    json!({"execution_id":old,"case_status":"evicted_for_priority","case_size":null,"case_detail":null}),
                );
            }
            if !self.snapshot_slot_available(step) {
                if optional {
                    self.entries.get_mut(id).unwrap().decision = Some("skipped_queue_full".into());
                    return;
                }
                size = CaptureSize::Bare;
            }
        }
        let ex = &self.entries[id];
        let start = ex.start.started_at;
        let selector = Selector::new(&ex.start.selector.field, &ex.start.selector.value);
        let overlaps = || {
            self.entries.iter().filter(|(peer_id, peer)| {
                peer_id.as_str() != id
                    && peer.start.started_at <= end
                    && peer.end.as_ref().is_none_or(|e| e.ended_at >= start)
            })
        };
        let window_start = if size == CaptureSize::Minimal {
            start
        } else {
            overlaps()
                .map(|(_, peer)| peer.start.started_at)
                .min()
                .map_or(start, |at| start.min(at))
        };
        let snapshot = CaptureSnapshot::for_execution_size(d, selector, window_start, end, size);
        if optional && snapshot.size == CaptureSize::Bare {
            self.entries.get_mut(id).unwrap().decision = Some("skipped_pool_exhausted".into());
            return;
        }
        let case_detail = match (size, snapshot.size) {
            (CaptureSize::Bare, _) => Some("capture_queue_full"),
            (_, CaptureSize::Bare) => Some("capture_pool_exhausted"),
            (_, CaptureSize::Minimal) if reason != FAULT => Some("failure_limit"),
            _ => None,
        };
        let size = snapshot.size;
        let peers: Vec<_> = if size == CaptureSize::Full {
            let mut overlaps: Vec<_> = overlaps().collect();
            overlaps.sort_unstable_by_key(|(peer_id, peer)| (peer.start.started_at, *peer_id));
            overlaps
                .into_iter()
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
                .collect()
        } else {
            Vec::new()
        };
        let snapshot_at = snapshot.captured_at();
        let mut metadata = json!({
            "run_id": self.config.run_id,
            "domain": d.id().as_str(),
            "execution_id": id,
            "name": ex.start.name,
            "reason": reason,
            "case_size": size.as_str(),
            "case_detail": case_detail,
            "run_fields": (size != CaptureSize::Bare).then_some(&self.config.fields),
            "display_timezone": self.config.display_timezone,
            "expanded_cases": self.config.expanded_cases,
            "window_start":window_start,
            "selector": ex.start.selector,
            "started_at": start,
            "ended_at": ex.end.as_ref().map(|e|e.ended_at),
            "verdict": ex.end.as_ref().map(|e|e.verdict),
            "end": ex.end.as_ref().filter(|_| size != CaptureSize::Bare),
            "peers": peers,
            "estimated_ms": ex.start.estimated_ms,
            "snapshot_delayed": delayed,
            "captured_at": snapshot_at,
            "grace_cutoff": ex.coverage["grace_cutoff"],
        });
        if reason == FAULT {
            let fault = self.telemetry_fault(id);
            metadata["fault"] = fault["fault"].clone();
            metadata["telemetry_limit"] = fault["telemetry_limit"].clone();
        }
        let ex = self.entries.get_mut(id).unwrap();
        ex.coverage["case_snapshot_at"] = json!(snapshot_at);
        ex.case_size = Some(size);
        ex.case_detail = case_detail;
        self.order += 1;
        if is_slow {
            self.slow += 1;
        }
        let ready = if size == CaptureSize::Bare || reason == FAULT {
            now
        } else {
            ex.capture_deadline.expect("set by capture caller")
        };
        if let Some(group) = group.filter(|_| size != CaptureSize::Bare) {
            self.pending.get_mut(&group).unwrap().members.push((
                id.into(),
                ready,
                metadata,
                snapshot,
            ));
            self.entries.get_mut(id).unwrap().decision = Some("pending".into());
            return;
        }
        self.pending.insert(
            id.into(),
            Pending {
                snapshot,
                group_until: (size == CaptureSize::Full
                    && reason == "end_test_missing_at_deadline")
                    .then(|| now + Duration::from_secs(60)),
                members: Vec::new(),
                ready,
                reason,
                order: self.order,
                metadata,
                registry: {
                    let mut registry = if size == CaptureSize::Bare {
                        Vec::new()
                    } else {
                        reg.to_vec()
                    };
                    for path in ["/Build/commit", "/Build/profile", "/Action"] {
                        if let Some(value) = self
                            .config
                            .fields
                            .get(path)
                            .and_then(Value::as_str)
                            .filter(|_| size != CaptureSize::Bare)
                        {
                            registry.retain(|fact| fact.path != path);
                            registry.push(RegistryFact {
                                path: path.into(),
                                value: value.into(),
                                created_at: self.began_at,
                                validated_at: self.began_at,
                                ttl_secs: None,
                                expired: None,
                            });
                        }
                    }
                    registry
                },
            },
        );
        self.entries.get_mut(id).unwrap().decision = Some("pending".into());
    }

    pub fn observe_log(&mut self, log: &LogEntry) {
        let now = Instant::now();
        for p in self.pending.values_mut() {
            if now < p.ready {
                p.snapshot.observe_log(log);
            }
            for (_, deadline, _, snapshot) in &mut p.members {
                if now < *deadline {
                    snapshot.observe_log(log);
                }
            }
        }
        let ids: Vec<_> = self
            .matching_groups(Record::Log(log))
            .flat_map(|group| &self.quotas.groups[group].executions)
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
        for p in self.pending.values_mut() {
            if now < p.ready {
                p.snapshot.observe_span(span);
            }
            for (_, deadline, _, snapshot) in &mut p.members {
                if now < *deadline {
                    snapshot.observe_span(span);
                }
            }
        }
        let ids: Vec<_> = self
            .matching_groups(Record::Span(span))
            .flat_map(|group| &self.quotas.groups[group].executions)
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
        let (deadline, received_at, cutoff) = self.grace_window();
        let ex = self.entries.get_mut(id).unwrap();
        ex.coverage["coverage_as_of"] = json!(received_at);
        ex.coverage["grace_cutoff"] = json!(cutoff);
        ex.register_loss(d);
        ex.forced = closing;
        if !closing {
            ex.timed_out = true;
        }
        if closing {
            ex.deadline = Some(deadline);
            self.grace.insert(id.into());
        }
        self.finish_telemetry(id);
        let ex = &mut self.entries.get_mut(id).unwrap();
        if ex.decision.is_none() {
            ex.capture_deadline = Some(deadline);
            let end = if closing { ex.last_seen } else { Utc::now() };
            self.admit(d, id, reason, end, false, reg);
        }
        let ex = &self.entries[id];
        let mut data = ex.summary_data();
        self.add_telemetry(id, &mut data);
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
        self.quotas.clear_tails();
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
        let mut delay = if self.writing.is_empty() && self.tail_writer.pending == 0 {
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
        for capture in self.pending.values() {
            if self.writer_available(capture.snapshot.size) {
                delay = delay.min(
                    capture
                        .ready_at(self.closing)
                        .saturating_duration_since(now),
                );
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

    fn publish_case_result(&mut self, id: &str, result: Result<&std::path::Path, &str>) {
        let ex = &self.entries[id];
        let affected = if ex.reason == Some(FAULT) {
            let group = &mut self.quotas.groups[ex.quota];
            match result {
                Ok(path) => group.case_path = Some(path.into()),
                Err(error) => group.case_error = Some(error.into()),
            }
            group
                .executions
                .iter()
                .filter(|peer| self.entries[*peer].reason == Some(FAULT))
                .cloned()
                .collect()
        } else {
            vec![id.to_owned()]
        };
        for id in affected {
            let ex = &self.entries[&id];
            match result {
                Ok(path) => self.append("case-ready", json!({"execution_id":id,"case_status":"ready","case_path":path,
                    "case_size":ex.case_size.map(CaptureSize::as_str),"case_detail":ex.case_detail})),
                Err(error) => self.append("case-error", json!({"execution_id":id,"case_status":"error","message":error})),
            }
        }
    }

    pub fn tick(&mut self, d: &Arc<Domain>, now: Instant) {
        for (id, result) in self.tail_writer.collect() {
            if let Err(error) = result {
                if let Some(info) = &mut self.entries.get_mut(&id).unwrap().telemetry_end {
                    info.as_object_mut().unwrap().remove("telemetry_tail_path");
                    info["telemetry_tail_error"] = json!(error);
                }
                self.append(
                    "telemetry-tail-error",
                    json!({"execution_id":id,"telemetry_tail_error":error}),
                );
            }
        }
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
            ex.coverage["coverage_as_of"] = ex.coverage["grace_cutoff"].clone();
            ex.coverage["finalized_at"] = json!(Utc::now());
            let data = ex.summary_data();
            ex.summary = None;
            self.grace.remove(&id);
            self.append("summary-final", data);
        }
        while let Ok((id, result)) = self.finished_rx.try_recv() {
            self.writing.remove(&id);
            let member_ids = self.writing_members.remove(&id).unwrap_or_default();
            let relative = result
                .as_ref()
                .map_err(String::as_str)
                .and_then(|path| {
                    path.strip_prefix(self.journal.path.parent().unwrap())
                        .map_err(|_| "case outside run directory")
                })
                .map(std::path::Path::to_path_buf);
            for id in std::iter::once(id).chain(member_ids) {
                self.publish_case_result(&id, relative.as_deref().map_err(|&error| error));
            }
        }
        loop {
            let candidate = self
                .pending
                .iter()
                .filter(|(_, p)| {
                    self.writer_available(p.snapshot.size) && p.ready_at(self.closing) <= now
                })
                .min_by_key(|(_, p)| {
                    (
                        p.snapshot.size != CaptureSize::Bare,
                        p.reason == "slow_pass",
                        p.order,
                    )
                })
                .map(|(id, _)| id.clone());
            let Some(id) = candidate else { break };
            let mut p = self.pending.remove(&id).unwrap();
            if p.snapshot.size != CaptureSize::Bare {
                let start = serde_json::from_value(p.metadata["started_at"].clone())
                    .expect("execution start");
                let end = serde_json::from_value(p.metadata["ended_at"].clone())
                    .unwrap_or(p.snapshot.captured_at());
                for row in self.samples.window(start, end, true) {
                    p.snapshot.add_machine_sample(row.clone());
                }
            }
            let tx = self.finished_tx.clone();
            let key = id.clone();
            let members: Vec<_> = p.members.iter().map(|m| m.0.clone()).collect();
            let dir = self.journal.path.parent().unwrap().join("cases");
            match std::thread::Builder::new()
                .name("test-case-writer".into())
                .spawn(move || {
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.write(&dir)))
                            .unwrap_or_else(|_| Err("case writer panicked".into()));
                    let _ = tx.send((key, result));
                }) {
                Ok(_) => {
                    self.writing_members.insert(id.clone(), members);
                    self.writing.insert(id);
                }
                Err(e) => {
                    let error = e.to_string();
                    for id in std::iter::once(id).chain(members) {
                        self.publish_case_result(&id, Err(&error));
                    }
                }
            }
        }
        if self.closing
            && !self.close_written
            && self.grace.is_empty()
            && self.pending.is_empty()
            && self.writing.is_empty()
            && self.tail_writer.pending == 0
        {
            self.close_written = true;
            self.append(
                "run-settled",
                json!({"at":Utc::now(),"executions":self.entries.len()}),
            );
        }
    }

    pub fn is_settled(&self) -> bool {
        self.close_written
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
        out["pending_telemetry_tails"] = json!(self.tail_writer.pending);
        out["pending_summaries"] = json!(self.grace.len() + self.active.len());
        let now = Instant::now();
        let mut pending = Vec::with_capacity(self.pending.len() + self.writing.len());
        for (id, capture) in &self.pending {
            for id in std::iter::once(id).chain(capture.members.iter().map(|m| &m.0)) {
                pending.push(json!({
                    "execution_id": id,
                    "name": self.entries[id].start.name,
                    "case_size": self.entries[id].case_size.map(CaptureSize::as_str),
                    "state": if capture.ready > now {"in_grace"} else {"queued"},
                }));
            }
        }
        for id in self
            .writing
            .iter()
            .chain(self.writing_members.values().flatten())
        {
            pending.push(json!({
                "execution_id": id,
                "name": self.entries[id].start.name,
                "case_size": self.entries[id].case_size.map(CaptureSize::as_str),
                "state":"writing",
            }));
        }
        out["pending_cases"] = json!(pending);
        out
    }
}
