use super::*;
use crate::daemon::domain::{DomainConfig, DomainId, DomainSource};
use crate::gelf::message::{Level, LogEntry};
use crate::span::types::SpanStatus;
use std::time::Duration;
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    domain: Arc<Domain>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_capacity(20)
    }
    fn with_capacity(capacity: usize) -> Self {
        let domain = Arc::new(Domain::new(
            DomainConfig {
                name: DomainId::new("tests").unwrap(),
                gelf_port: 0,
                otlp_grpc_port: 0,
                otlp_http_port: 0,
                log_buffer_size: capacity,
                span_buffer_size: capacity,
                source: DomainSource::Ephemeral,
            },
            0,
        ));
        Self {
            dir: TempDir::new().unwrap(),
            domain,
        }
    }
    fn rpc(&self, m: &str, p: Value) -> Value {
        self.domain
            .test_log
            .rpc(&self.domain, &format!("test_log.{m}"), &p, vec![])
            .unwrap()
    }
    fn begin(&self, run: &str, grace: u64) -> Value {
        self.rpc("begin",json!({"run_id":run,"jsonl_path":self.dir.path().join(format!("{run}.jsonl")),"grace_period_ms":grace}))
    }
    fn event(&self, id: &str) -> Value {
        json!({"run_id":"one","execution_id":id,"name":format!("chain-{id}"),"selector":{"field":"trace_id","value":format!("{:032x}",if id=="a"{1}else{2})},"started_at":Utc::now()})
    }
    fn log(&self, trace: u128, msg: &str) {
        self.log_at(trace, Level::Info, msg);
    }
    fn log_at(&self, trace: u128, level: Level, msg: &str) {
        let mut e = LogEntry::synthetic(level, msg);
        e.seq = self.domain.pipeline.assign_seq();
        e.trace_id = Some(trace);
        self.domain.pipeline.append_to_store(e.clone());
        self.domain.test_log.observe_log(&e);
    }
    fn close(&self, run: &str) -> Value {
        let limit = Instant::now() + Duration::from_secs(4);
        loop {
            let status = self.rpc("close", json!({"run_id":run,"wait_timeout_ms":0}));
            if status["finalization"] == "settled" && status["persistence"] == "complete" {
                return status;
            }
            assert!(Instant::now() < limit, "{status}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn rows(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("one.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    fn end_as(&self, mut event: Value, verdict: &str) -> Value {
        event["ended_at"] = json!(Utc::now());
        event["verdict"] = json!(verdict);
        self.rpc("end", event)
    }
    fn case_row(&self, id: &str) -> Value {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(row) = self
                .rows()
                .into_iter()
                .find(|r| r["event"] == "case-ready" && r["execution_id"] == id)
            {
                return row;
            }
            assert!(Instant::now() < until, "no ready case for {id}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn case_logs(&self, row: &Value) -> Vec<LogEntry> {
        let path = self.dir.path().join(row["case_path"].as_str().unwrap());
        crate::cases::load::load(&path).unwrap().logs.unwrap()
    }
    fn with_run(&self, f: impl FnOnce(&mut Run)) {
        f(self
            .domain
            .test_log
            .state
            .lock()
            .unwrap()
            .run
            .as_mut()
            .unwrap());
    }
}

fn ingest_quota_log(f: &Fixture, mut row: LogEntry) -> LogEntry {
    crate::daemon::log_processor::process_entry_for_domain(
        &mut row,
        &f.domain.pipeline,
        &crate::daemon::session::SessionRegistry::new(),
        f.domain.id(),
    );
    row
}

fn quota_log(f: &Fixture, trace: u128, level: Level, message: &str) -> LogEntry {
    let mut row = LogEntry::synthetic(level, message);
    row.trace_id = Some(trace);
    ingest_quota_log(f, row)
}

fn quota_span(f: &Fixture, trace: u128, id: u64) {
    quota_span_with_status(f, trace, id, SpanStatus::Ok);
}

fn quota_span_with_status(f: &Fixture, trace: u128, id: u64, status: SpanStatus) {
    use crate::span::types::{SpanEntry, SpanKind};
    let now = Utc::now();
    let mut span = SpanEntry {
        seq: 0,
        trace_id: trace,
        span_id: id,
        parent_span_id: None,
        start_time: now,
        end_time: now,
        duration_ms: 0.0,
        name: "test span".into(),
        kind: SpanKind::Internal,
        service_name: "test".into(),
        status,
        attributes: Default::default(),
        events: Vec::new(),
    };
    crate::daemon::span_processor::process_span_for_domain(
        &mut span,
        &f.domain.span_store,
        &crate::daemon::session::SessionRegistry::new(),
        &f.domain.pipeline,
        &crate::collector::registry::CollectorRegistry::new(),
        f.domain.id(),
    );
}

#[test]
fn reader_growable_ring_uses_its_maximum_and_fixed_ring_has_a_minimum_quota() {
    for (maximum, quota, ring_size) in [(Some(8), 2, 4), (None, 1, 2)] {
        let f = Fixture::with_capacity(2);
        f.domain.pipeline.store.resize_with(Some(2), maximum, || ());
        f.begin("one", 0);
        let execution = f.event("a");
        f.rpc("start", execution.clone());
        quota_log(&f, 9, Level::Info, "neighbor");
        for _ in 0..quota {
            quota_log(&f, 1, Level::Info, "target");
        }
        assert_eq!(
            f.domain.pipeline.logs_by_trace_id(1).len(),
            quota,
            "maximum {maximum:?}"
        );
        assert_eq!(f.domain.pipeline.store.ring_info().0.current, ring_size);
        quota_log(&f, 1, Level::Info, "crossing");
        let ack = f.end_as(execution, "OK");
        assert_eq!(ack["telemetry_limit"]["limit"], quota);
        assert_eq!(ack["telemetry_limit"]["ring_size"], maximum.unwrap_or(2));
        f.close("one");
    }
}

fn crossing_under_slot_pressure(name: &str, size: &str, detail: Option<&str>) {
    let f = Fixture::new();
    f.begin("one", 60_000);
    for i in 0..7 {
        assert_eq!(
            f.end_as(f.event(&format!("test-{i}")), "FAIL")["case_size"],
            "full"
        );
    }
    let mut execution = f.event("a");
    execution["name"] = json!(name);
    f.rpc("start", execution);
    for _ in 0..6 {
        quota_log(&f, 1, Level::Info, "target");
    }
    let row = f.case_row("a");
    assert_eq!(row["case_size"], size, "{name} crossing");
    assert_eq!(row["case_detail"].as_str(), detail);
}

#[test]
fn reader_crossing_without_a_test_slot_writes_a_bare_case() {
    crossing_under_slot_pressure("ordinary test", "bare", Some("capture_queue_full"));
}

#[test]
fn reader_step_crossing_uses_the_reserved_slot() {
    crossing_under_slot_pressure("step:build", "minimal", None);
}

#[test]
fn reader_tail_write_failure_is_reported_and_the_run_settles() {
    let f = Fixture::new();
    f.begin("one", 0);
    let execution = f.event("a");
    f.rpc("start", execution.clone());
    for _ in 0..6 {
        quota_log(&f, 1, Level::Info, "target");
    }
    let case = f.case_row("a");
    let case_path = f.dir.path().join(case["case_path"].as_str().unwrap());
    let case_dir = case_path.parent().unwrap();
    std::fs::rename(case_dir, f.dir.path().join("saved-cases")).unwrap();
    std::fs::write(case_dir, b"blocks the tail file").unwrap();
    assert!(f.end_as(execution, "OK")["telemetry_tail_path"].is_string());
    assert_eq!(f.close("one")["finalization"], "settled");
    assert!(
        f.rows()
            .iter()
            .any(|row| row["event"] == "telemetry-tail-error"
                && row["execution_id"] == "a"
                && row["telemetry_tail_error"].is_string()),
        "tail write error missing from the settled journal"
    );
}

#[test]
fn reader_log_crossing_applies_post_crossing_rules_to_spans() {
    let f = Fixture::new();
    f.begin("one", 0);
    let execution = f.event("a");
    f.rpc("start", execution.clone());
    quota_span(&f, 1, 1);
    for _ in 0..6 {
        quota_log(&f, 1, Level::Info, "target");
    }
    quota_span(&f, 1, 2);
    quota_span_with_status(&f, 1, 3, SpanStatus::Error("kept".into()));
    let spans = f.domain.span_store.get_trace(1);
    assert_eq!(
        spans.iter().map(|span| span.span_id).collect::<Vec<_>>(),
        [1, 3]
    );
    let ack = f.end_as(execution, "OK");
    assert_eq!(ack["telemetry_limit"]["kind"], "logs");
    assert_eq!(ack["refused_telemetry"], json!({"logs":1,"spans":1}));
    let ack: TestLogAck = serde_json::from_value(ack).unwrap();
    assert_eq!(ack.refused_telemetry.unwrap().spans, 1);
    f.close("one");
}

#[test]
fn reader_sequential_executions_share_the_run_quota_and_crossing_case() {
    let f = Fixture::new();
    f.begin("one", 0);
    let first = f.event("a");
    f.rpc("start", first.clone());
    for _ in 0..3 {
        quota_log(&f, 1, Level::Info, "first execution");
    }
    assert!(f.end_as(first.clone(), "OK")["fault"].is_null());

    let event = |id: &str| {
        let mut execution = f.event(id);
        execution["selector"] = first["selector"].clone();
        execution
    };
    let second = event("second");
    f.rpc("start", second.clone());
    for _ in 0..3 {
        quota_log(&f, 1, Level::Info, "second execution");
    }
    assert_eq!(f.end_as(second, "OK")["fault"], "excessive_telemetry");
    assert_eq!(f.domain.pipeline.logs_by_trace_id(1).len(), 5);
    let crossing = f.case_row("second");

    let later = event("later");
    f.rpc("start", later.clone());
    assert_eq!(f.case_row("later")["case_path"], crossing["case_path"]);
    assert!(f.rows().iter().any(|row| row["event"] == "telemetry-fault"
        && row["execution_id"] == "later"
        && row["fault"] == "excessive_telemetry"));
    quota_log(&f, 1, Level::Info, "refused on first arrival");
    let ack = f.end_as(later, "FAIL");
    assert_eq!(ack["refused_telemetry"]["logs"], 2);
    f.close("one");
    assert_eq!(
        std::fs::read_dir(f.dir.path().join("cases"))
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "zip"))
            .count(),
        1
    );
}

#[test]
fn ruling_14_crossing_reports_all_shared_executions_and_keeps_old_rows() {
    let f = Fixture::new();
    f.begin("one", 3000);
    let a = f.event("a");
    let mut b = a.clone();
    b["execution_id"] = json!("b");
    b["name"] = json!("other execution with the same trace");
    f.rpc("start", a.clone());
    f.rpc("start", b.clone());
    quota_log(&f, 9, Level::Info, "neighbor");
    for _ in 0..6 {
        quota_log(&f, 1, Level::Info, "target");
    }
    let case = f.case_row("a");
    assert_eq!(f.case_row("b")["case_path"], case["case_path"]);
    let archive = f.case_logs(&case);
    assert_eq!(archive.iter().filter(|l| l.message == "target").count(), 5);
    assert_eq!(f.domain.pipeline.logs_by_trace_id(1).len(), 5);
    assert_eq!(f.domain.pipeline.logs_by_trace_id(9).len(), 1);
    assert_eq!(
        f.domain.test_log.status("tests", None)["test_log"]["faults"]["excessive_telemetry"],
        2
    );
    f.end_as(a, "OK");
    f.end_as(b, "OK");
    f.close("one");
}

#[test]
fn ruling_15_ingest_refusal_keeps_rows_out_of_ring_and_prebuffer_but_keeps_shared_rows() {
    let f = Fixture::new();
    f.begin("one", 0);
    f.domain.pipeline.resize_pre_buffer(20);
    let a = f.event("a");
    let mut b = f.event("b");
    b["selector"] = json!({"field":"worker","value":"other"});
    f.rpc("start", a.clone());
    f.rpc("start", b.clone());
    for _ in 0..5 {
        quota_log(&f, 1, Level::Info, "kept");
    }
    let refused = quota_log(&f, 1, Level::Info, "refused");
    assert!(!f.domain.pipeline.contains_seq(refused.seq));
    assert!(!f
        .domain
        .pipeline
        .pre_buffer_entries_by_trace_id(1)
        .iter()
        .any(|r| r.seq == refused.seq));
    let mut shared = LogEntry::synthetic(Level::Info, "shared");
    shared.trace_id = Some(1);
    shared
        .additional_fields
        .insert("worker".into(), json!("other"));
    let shared = ingest_quota_log(&f, shared);
    assert!(f.domain.pipeline.contains_seq(shared.seq));
    f.end_as(a, "OK");
    f.end_as(b, "OK");
    f.close("one");
    let end = f
        .rows()
        .into_iter()
        .find(|r| r["execution_id"] == "a" && r["event"] == "end-test")
        .unwrap();
    assert_eq!(end["refused_telemetry"]["logs"], 1);
    assert_eq!(
        end["coverage"]["domain_loss_during_window"]["logs_dropped"],
        0
    );
}

#[test]
fn ruling_16_ok_verdict_keeps_the_fault_refusal_count_and_tail_reference() {
    let f = Fixture::new();
    f.begin("one", 0);
    let a = f.event("a");
    f.rpc("start", a.clone());
    for _ in 0..7 {
        quota_log(&f, 1, Level::Info, "target");
    }
    let ack = f.end_as(a, "OK");
    assert_eq!(ack["fault"], "excessive_telemetry");
    f.close("one");
    let end = f
        .rows()
        .into_iter()
        .find(|r| r["execution_id"] == "a" && r["event"] == "end-test")
        .unwrap();
    assert_eq!(end["verdict"], "OK");
    assert_eq!(end["fault"], "excessive_telemetry");
    assert_eq!(end["refused_telemetry"]["logs"], 2);
    assert!(end["telemetry_tail_path"].is_string());
}

#[test]
fn ruling_17_exact_limits_are_independent_and_the_next_span_crosses() {
    let f = Fixture::new();
    f.begin("one", 0);
    let a = f.event("a");
    f.rpc("start", a.clone());
    for n in 1..=5 {
        quota_log(&f, 1, Level::Info, "target");
        quota_span(&f, 1, n);
    }
    let count = || {
        f.domain.test_log.status("tests", None)["test_log"]["faults"]["excessive_telemetry"]
            .as_u64()
            .unwrap_or(0)
    };
    assert_eq!(count(), 0);
    quota_span(&f, 1, 6);
    assert_eq!(count(), 1);
    assert_eq!(f.domain.span_store.get_trace(1).len(), 5);
    f.end_as(a, "OK");
    f.close("one");
}

#[test]
fn ruling_18_a_full_ring_does_not_fault_its_largest_under_limit_selector() {
    let f = Fixture::new();
    f.begin("one", 0);
    let a = f.event("a");
    f.rpc("start", a.clone());
    for trace in 2..18 {
        quota_log(&f, trace, Level::Info, "neighbor");
    }
    for _ in 0..4 {
        quota_log(&f, 1, Level::Info, "target");
    }
    assert_eq!(f.domain.pipeline.store_len(), 20);
    let count = || {
        f.domain.test_log.status("tests", None)["test_log"]["faults"]["excessive_telemetry"]
            .as_u64()
            .unwrap_or(0)
    };
    assert_eq!(count(), 0);
    quota_log(&f, 1, Level::Info, "at limit");
    assert_eq!(count(), 0);
    quota_log(&f, 1, Level::Info, "crossing");
    assert_eq!(count(), 1);
    f.end_as(a, "OK");
    f.close("one");
}

#[test]
fn ruling_6_tail_writes_the_newest_two_hundred_at_end() {
    let f = Fixture::new();
    f.begin("one", 0);
    let a = f.event("a");
    f.rpc("start", a.clone());
    for _ in 0..5 {
        quota_log(&f, 1, Level::Info, "held");
    }
    for i in 0..250 {
        quota_log(&f, 1, Level::Info, &format!("tail-{i}"));
        quota_span(&f, 1, i + 1);
    }
    f.end_as(a, "OK");
    f.close("one");
    let end = f
        .rows()
        .into_iter()
        .find(|r| r["execution_id"] == "a" && r["event"] == "end-test")
        .unwrap();
    let path = f.dir.path().join(
        end["telemetry_tail_path"]
            .as_str()
            .expect("tail path on end"),
    );
    let rows: Vec<Value> = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let messages: Vec<_> = rows
        .iter()
        .filter_map(|r| r.pointer("/record/message").and_then(Value::as_str))
        .collect();
    assert_eq!(messages.len(), 200);
    assert_eq!(messages[0], "tail-50");
    assert_eq!(messages[199], "tail-249");
    let span_ids: Vec<_> = rows
        .iter()
        .filter_map(|r| r.pointer("/record/span_id").and_then(Value::as_str))
        .collect();
    assert_eq!(span_ids.len(), 200);
    assert_eq!(span_ids[0], "0000000000000033");
    assert_eq!(span_ids[199], "00000000000000fa");
}

#[test]
fn ruling_6_all_error_selector_stops_after_the_thousandth_exemption() {
    for (capacity, retained) in [(4096, 2024), (20, 10)] {
        let f = Fixture::with_capacity(capacity);
        f.begin("one", 0);
        let a = f.event("a");
        f.rpc("start", a.clone());
        for i in 1..=retained + 1 {
            quota_log(&f, 1, Level::Error, &format!("error-{i}"));
        }
        let held = f.domain.pipeline.logs_by_trace_id(1);
        assert_eq!(held.len(), retained, "ring {capacity}");
        assert_eq!(held.last().unwrap().message, format!("error-{retained}"));
        f.end_as(a, "OK");
        f.close("one");
        let end = f
            .rows()
            .into_iter()
            .find(|r| r["execution_id"] == "a" && r["event"] == "end-test")
            .unwrap();
        assert_eq!(end["refused_telemetry"]["logs"], 1);
        let tail = f
            .dir
            .path()
            .join(end["telemetry_tail_path"].as_str().unwrap());
        let tail = std::fs::read_to_string(tail).unwrap();
        assert!(tail.contains(&format!("\"message\":\"error-{retained}\"")));
        assert!(tail.contains(&format!("\"message\":\"error-{}\"", retained + 1)));
    }
}

#[test]
fn ruling_7_tail_budget_is_shared_and_shortens_oldest_tails() {
    let f = Fixture::new();
    f.begin("one", 0);
    let large = "x".repeat(64 * 1024);
    let mut events = Vec::new();
    for trace in 1..=20 {
        let mut execution = f.event(&format!("test-{trace}"));
        execution["selector"]["value"] = json!(format!("{trace:032x}"));
        f.rpc("start", execution.clone());
        for _ in 0..25 {
            quota_log(&f, trace, Level::Info, &large);
        }
        events.push(execution);
    }
    for execution in events {
        f.end_as(execution, "OK");
    }
    f.close("one");
    let endings: Vec<_> = f
        .rows()
        .into_iter()
        .filter(|r| r["event"] == "end-test")
        .collect();
    let first = &endings[0];
    let pool = first["telemetry_tail_pool_bytes"]
        .as_u64()
        .expect("shared budget bytes");
    assert!(
        pool > 15 * 1024 * 1024 && pool <= 16 * 1024 * 1024,
        "{pool}"
    );
    assert_eq!(first["telemetry_tail_shortened"]["logs"], 20);
    assert_eq!(
        endings.last().unwrap()["telemetry_tail_shortened"]["logs"],
        0
    );
}

#[test]
fn failed_chain_keeps_peer_and_assertion_in_portable_case() {
    let f = Fixture::new();
    assert_eq!(f.begin("one", 25)["status"], "accepted");
    let mut a = f.event("a");
    let mut b = f.event("b");
    f.rpc("start", a.clone());
    f.rpc("start", b.clone());
    f.log(1, "peer");
    f.log(2, "target");
    f.rpc(
        "machine_load",
        json!({"run_id":"one","host":"host-a","ts":Utc::now(),"fields":{"own":17}}),
    );
    b["verdict"] = json!("FAIL");
    b["ended_at"] = json!(Utc::now());
    b["fields"] = json!({"message":"broken expectation"});
    assert_eq!(f.rpc("end", b.clone())["case_status"], "pending");
    let replay = f.rpc("replay", json!({"run_id":"one","events":[b,{}]}));
    assert_eq!(replay["results"][0]["status"], "already_applied");
    assert_eq!(replay["results"][1]["status"], "refused");
    f.log(2, "batched");
    a["verdict"] = json!("OK");
    a["ended_at"] = json!(Utc::now());
    f.rpc("end", a);
    f.close("one");
    let rows = f.rows();
    let p = rows.iter().find(|r| r["event"] == "case-ready").unwrap()["case_path"]
        .as_str()
        .unwrap();
    assert!(p.starts_with("cases/"));
    let archive = crate::cases::load::load(&f.dir.path().join(p)).unwrap();
    let messages: Vec<_> = archive
        .logs
        .as_ref()
        .unwrap()
        .iter()
        .map(|e| e.message.as_str())
        .collect();
    for m in [
        "peer",
        "target",
        "batched",
        "machine-load",
        "broken expectation",
    ] {
        assert!(messages.contains(&m), "{m}: {messages:?}")
    }
    assert_eq!(
        rows.iter().filter(|r| r["event"] == "case-ready").count(),
        1
    );
    assert_eq!(
        rows.iter()
            .filter(|r| r["event"] == "summary-final")
            .count(),
        2
    );
}

#[test]
fn competing_run_is_refused_and_seal_cannot_close_successor() {
    let f = Fixture::new();
    f.begin("one", 0);
    let a = f.event("a");
    f.rpc("start", a);
    assert_eq!(f.begin("two", 0)["code"], "run_active");
    assert_eq!(
        f.rpc("seal", json!({"run_id":"two"}))["code"],
        "wrong_run_id"
    );
    f.rpc("seal", json!({"run_id":"one"}));
    f.close("one");
    assert_eq!(f.begin("two", 0)["status"], "accepted");
    assert_eq!(
        f.rpc("seal", json!({"run_id":"one"}))["finalization"],
        "settled"
    );
    let mut b = f.event("b");
    b["run_id"] = json!("two");
    assert_eq!(f.rpc("start", b)["status"], "accepted");
    let row = f
        .rows()
        .into_iter()
        .find(|r| r["event"] == "incomplete")
        .unwrap();
    assert!(row["verdict"].is_null() && row["elapsed_ms"].is_null());
}

#[test]
fn unconfirmed_start_backfills_without_double_counting_or_spending_an_extra_case_slot() {
    let f = Fixture::new();
    let path = f.dir.path().join("one.jsonl");
    f.rpc(
        "begin",
        json!({"run_id":"one","jsonl_path":path,"max_failures":0,"grace_period_ms":0,"summary_sample_percent":0}),
    );
    let mut a = f.event("a");
    f.log(1, "already stored");
    a["ended_at"] = json!(Utc::now());
    a["verdict"] = json!("FAIL");
    let result = f.rpc("replay", json!({"run_id":"one","events":[a.clone(),a]}));
    assert_eq!(result["results"][0]["status"], "applied");
    assert_eq!(result["results"][0]["case_size"], "minimal");
    assert_eq!(result["results"][1]["status"], "already_applied");
    f.close("one");
    let row = f
        .rows()
        .into_iter()
        .find(|r| r["event"] == "summary-final")
        .unwrap();
    assert_eq!(row["coverage"]["collection_mode"], "ring_backfill");
    assert_eq!(
        row["summary"]["log_levels"]
            .as_object()
            .unwrap()
            .values()
            .filter_map(Value::as_u64)
            .sum::<u64>(),
        1
    );
}

#[test]
fn slow_cases_yield_to_failures_and_green_reruns_have_their_own_slots() {
    let f = Fixture::new();
    f.rpc(
        "begin",
        json!({"run_id":"one","jsonl_path":f.dir.path().join("one.jsonl"),
        "grace_period_ms":3000,"max_failures":1,"max_rerun_cases":1,"max_slow_pass_cases":20}),
    );
    let attempt = |id: &str, verdict: &str, rerun: Option<&str>| {
        let mut e = f.event(id);
        e["slow_if"] = json!("elapsed > 0ms");
        e["started_at"] = json!(Utc::now() - chrono::Duration::seconds(1));
        e["rerun_of"] = json!(rerun);
        f.rpc("start", e.clone());
        e["ended_at"] = json!(Utc::now());
        e["verdict"] = json!(verdict);
        f.rpc("end", e)
    };
    for i in 0..7 {
        assert_eq!(
            attempt(&format!("ok-{i}"), "OK", None)["case_status"],
            "pending"
        );
    }
    assert_eq!(
        attempt("pressure", "OK", None)["case_status"],
        "skipped_queue_full"
    );
    assert_eq!(attempt("red", "FAIL", None)["case_status"], "pending");
    assert_eq!(attempt("another", "FAIL", None)["case_size"], "minimal");
    assert_eq!(
        attempt("isolated", "OK", Some("red"))["case_status"],
        "pending"
    );
    assert_eq!(
        attempt("isolated2", "OK", Some("another"))["case_status"],
        "skipped_rerun_limit"
    );
    let pending = f.rpc("close", json!({"run_id":"one","wait_timeout_ms":0}));
    assert_eq!(pending["finalization"], "pending");
    let ids: Vec<_> = pending["pending_cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["case_size"] != "bare")
        .map(|v| v["execution_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 7);
    assert!(ids.contains(&"red") && ids.contains(&"isolated"));
    assert!(!ids.contains(&"ok-6") && !ids.contains(&"ok-4"));
}

#[test]
fn timeout_keeps_unknown_verdict_and_a_later_pass_reuses_capture() {
    let f = Fixture::new();
    f.begin("one", 0);
    let mut a = f.event("a");
    a["test_timeout_ms"] = json!(1);
    f.rpc("start", a.clone());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !f.rows().iter().any(|v| v["event"] == "incomplete") {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let v = f.domain.test_log.status("tests", None)["test_log"].clone();
    assert_eq!(v["executions"]["by_verdict"]["NO_VERDICT"], 1);
    assert_eq!(v["executions"]["in_flight"], 0);
    assert_eq!(v["summaries"]["pending"], 1);
    a["verdict"] = json!("OK");
    a["ended_at"] = json!(Utc::now());
    f.rpc("end", a);
    f.close("one");
    let v = f.domain.test_log.status("tests", Some("one"))["test_log"].clone();
    assert_eq!(v["executions"]["ended"], 1);
    assert_eq!(v["executions"]["by_verdict"]["NO_VERDICT"], 0);
    assert_eq!(v["executions"]["by_verdict"]["OK"], 1);
    let rows = f.rows();
    assert_eq!(
        rows.iter().filter(|v| v["event"] == "case-ready").count(),
        1
    );
    assert!(rows
        .iter()
        .any(|v| v["event"] == "end-test" && v["verdict"] == "OK"));
}

#[test]
fn begin_refuses_to_replace_a_jsonl_file_owned_by_another_run() {
    let f = Fixture::new();
    f.begin("one", 0);
    f.close("one");
    let original = std::fs::read(f.dir.path().join("one.jsonl")).unwrap();
    let result = f.rpc(
        "begin",
        json!({"run_id":"two","jsonl_path":f.dir.path().join("one.jsonl")}),
    );
    assert_eq!(result["code"], "test_log_unwritable");
    assert_eq!(
        std::fs::read(f.dir.path().join("one.jsonl")).unwrap(),
        original
    );
}

#[test]
fn close_keeps_receipt_identity_immutable_and_stamps_fixed_coverage_cutoff() {
    let f = Fixture::new();
    f.begin("one", 30);
    let mut evt = f.event("a");
    f.rpc("start", evt.clone());
    let mut other = evt.clone();
    other["rerun_of"] = json!("other");
    assert_eq!(f.rpc("start", other)["code"], "execution_identity_conflict");
    evt["ended_at"] = json!(Utc::now());
    evt["verdict"] = json!("FAIL");
    evt["fields"] = json!({"message":"original"});
    assert_eq!(f.rpc("end", evt.clone())["status"], "accepted");
    f.close("one");
    for (key, value, code) in [
        ("name", json!("other"), "execution_identity_conflict"),
        ("rerun_of", json!("other"), "execution_identity_conflict"),
        (
            "started_at",
            json!(Utc::now()),
            "execution_identity_conflict",
        ),
        ("verdict", json!("OK"), "execution_outcome_conflict"),
        ("ended_at", json!(Utc::now()), "execution_outcome_conflict"),
        (
            "fields",
            json!({"message":"altered"}),
            "execution_outcome_conflict",
        ),
    ] {
        let mut changed = evt.clone();
        changed[key] = value;
        let result = f.rpc("replay", json!({"run_id":"one","events":[changed]}));
        assert_eq!(result["results"][0]["code"], code, "{key}");
    }
    assert_eq!(f.rpc("end", evt)["status"], "already_applied");
    let rows = f.rows();
    let coverage =
        |event: &str| rows.iter().find(|r| r["event"] == event).unwrap()["coverage"].clone();
    let initial = coverage("end-test");
    let final_revision = coverage("summary-final");
    let time = |v: &Value| chrono::DateTime::parse_from_rfc3339(v.as_str().unwrap()).unwrap();
    assert_eq!(initial["coverage_as_of"], initial["snapshot_at"]);
    assert_eq!(
        initial["case_snapshot_at"],
        final_revision["case_snapshot_at"]
    );
    assert_eq!(initial["grace_cutoff"], final_revision["coverage_as_of"]);
    assert_eq!(
        time(&initial["grace_cutoff"]) - time(&initial["snapshot_at"]),
        chrono::Duration::milliseconds(30)
    );
    assert!(time(&final_revision["finalized_at"]) >= time(&final_revision["coverage_as_of"]));
}

#[test]
fn sampled_out_missing_start_still_reports_snapshot_and_independent_ring_bounds() {
    let f = Fixture::new();
    f.rpc("begin", json!({"run_id":"one","jsonl_path":f.dir.path().join("one.jsonl"),"summary_sample_percent":0,"grace_period_ms":5}));
    let mut e = f.event("a");
    e["verdict"] = json!("OK");
    e["ended_at"] = json!(Utc::now());
    assert_eq!(f.rpc("end", e)["status"], "accepted");
    f.close("one");
    let row = f
        .rows()
        .into_iter()
        .find(|r| r["event"] == "summary-final")
        .unwrap();
    assert!(row["summary"].is_null());
    assert_eq!(row["coverage"]["collection_mode"], "ring_backfill");
    assert_eq!(
        row["coverage"]["coverage_as_of"],
        row["coverage"]["grace_cutoff"]
    );
    assert!(row["coverage"]["snapshot_at"].is_string());
    for key in ["log_coverage", "span_coverage"] {
        assert_eq!(row["coverage"][key]["status"], "retained");
    }
}

#[test]
fn ruling_11_capture_salvages_priority_prefix_and_counts_omissions() {
    let f = Fixture::new();
    f.begin("one", 0);
    let a = f.event("a");
    f.rpc("start", a.clone());
    for (trace, level, text) in [
        (9, Level::Info, "win-inf"),
        (9, Level::Error, "old-err"),
        (1, Level::Error, "own-err"),
        (9, Level::Warn, "win-wrn"),
        (1, Level::Info, "own-inf"),
        (9, Level::Error, "new-err"),
    ] {
        f.log_at(trace, level, text);
    }
    let record = crate::nightly::log_bytes(&LogEntry::synthetic(Level::Info, "win-inf"));
    f.domain
        .test_log
        .capture_pool
        .lock()
        .unwrap()
        .set_available(3 * (record + 96));
    assert_eq!(f.end_as(a, "FAIL")["case_status"], "pending");
    f.close("one");
    let case = f.case_row("a");
    assert_eq!(case["case_size"], "full");
    let logs = f.case_logs(&case);
    let mut messages: Vec<_> = logs.iter().map(|r| r.message.as_str()).collect();
    messages.sort_unstable();
    assert_eq!(messages, ["failure", "new-err", "own-err", "own-inf"]);
    let marker = logs.iter().find(|r| r.message == "failure").unwrap();
    assert_eq!(marker.additional_fields["coverage"]["cut"], true);
    assert_eq!(
        marker.additional_fields["coverage"]["omitted_records"],
        json!({"selector_error":0,"selector_other":0,"window_error":1,"window_warn":1,"window_other":1})
    );
}

#[test]
fn ruling_11_empty_capture_pool_writes_bare_case() {
    let f = Fixture::new();
    f.begin("one", 0);
    let a = f.event("a");
    f.rpc("start", a.clone());
    f.log(1, "own");
    f.domain
        .test_log
        .capture_pool
        .lock()
        .unwrap()
        .set_available(0);
    assert_eq!(f.end_as(a, "TIMEOUT")["case_size"], "bare");
    let row = f.case_row("a");
    assert_eq!(row["case_detail"], "capture_pool_exhausted");
    let logs = f.case_logs(&row);
    assert_eq!(logs.len(), 1);
    assert_eq!(
        logs[0].additional_fields["case_context"]["verdict"],
        "TIMEOUT"
    );
    assert_eq!(
        logs[0].additional_fields["coverage"]["omitted_records"]["selector_other"],
        1
    );
    f.close("one");
}

#[test]
fn ruling_12_plan_drives_the_default_full_failure_limit() {
    for (planned, want) in [
        (Some(40), 20),
        (Some(1_000), 100),
        (Some(7_000), 300),
        (None, 50),
    ] {
        let f = Fixture::new();
        let mut begin = json!({
            "run_id":"one","jsonl_path":f.dir.path().join("one.jsonl"),"grace_period_ms":60_000,
            "planned_executions":planned,
        });
        assert_eq!(f.rpc("begin", begin.clone())["status"], "accepted");
        let limit =
            || f.domain.test_log.status("tests", None)["test_log"]["cases"]["max_failures"].clone();
        assert_eq!(limit(), want, "planned {planned:?}");
        let fail = |number, size| {
            let id = format!("failure-{number}");
            let outcome = f.end_as(f.event(&id), "FAIL");
            assert_eq!(
                outcome["case_size"], size,
                "plan {planned:?}, failure {number}"
            );
            // Release the fixture's snapshot after each real admission. The
            // fixture is checking failure rank independently of queue pressure.
            f.with_run(|run| run.take_pending_for_test(&id, false));
        };
        for number in 1..=want {
            fail(number, "full");
        }
        fail(want + 1, "minimal");
        if planned.is_none() {
            begin["planned_executions"] = json!(1_000);
            assert_eq!(f.rpc("begin", begin)["status"], "accepted");
            assert_eq!(limit(), 100);
            for number in want + 2..=100 {
                fail(number, "full");
            }
            fail(101, "minimal");
        }
    }
}

#[test]
fn ruling_12_failed_reruns_get_cases_and_steps_do_not_spend_failure_limit() {
    let f = Fixture::new();
    f.rpc(
        "begin",
        json!({
            "run_id":"one","jsonl_path":f.dir.path().join("one.jsonl"),
            "grace_period_ms":0,"planned_executions":1_000,"max_failures":1,"max_rerun_cases":0,
        }),
    );
    let mut step = f.event("step");
    step["name"] = json!("step:5-rust");
    f.rpc("start", step.clone());
    assert_eq!(f.end_as(step, "ABORTED")["case_size"], "full");

    let mut failed = f.event("a");
    failed["rerun_of"] = json!("original");
    f.rpc("start", failed.clone());
    f.log(1, "target");
    assert_eq!(f.end_as(failed, "FAIL")["case_size"], "full");

    let mut repeat = f.event("b");
    repeat["rerun_of"] = json!("original");
    f.rpc("start", repeat.clone());
    f.log(2, "selector only");
    f.log(9, "outside selector");
    assert_eq!(f.end_as(repeat, "TIMEOUT")["case_size"], "minimal");

    let mut clean = f.event("clean");
    clean["rerun_of"] = json!("original");
    assert_eq!(f.end_as(clean, "OK")["case_status"], "skipped_rerun_limit");
    f.close("one");
    let logs = f.case_logs(&f.case_row("b"));
    assert_eq!(
        logs.iter()
            .map(|row| row.message.as_str())
            .collect::<Vec<_>>(),
        ["selector only", "failure"]
    );
}

#[test]
fn ruling_13_step_has_reserved_slot_and_excess_ends_get_bare_cases() {
    let f = Fixture::new();
    f.begin("one", 3000);
    for i in 0..7 {
        assert_eq!(
            f.end_as(f.event(&format!("test-{i}")), "FAIL")["case_size"],
            "full",
            "test ending {} must use a full slot",
            i + 1,
        );
    }
    let test = f.event("a");
    f.rpc("start", test.clone());
    f.log(1, "selector evidence outside bare case");
    assert_eq!(
        f.end_as(test, "FAIL")["case_size"],
        "bare",
        "eighth test ending"
    );
    let mut step = f.event("step");
    step["name"] = json!("step:5-rust");
    assert_eq!(
        f.end_as(step, "ABORTED")["case_size"],
        "full",
        "reserved step ending"
    );
    let mut next_step = f.event("next-step");
    next_step["name"] = json!("step:6-dsl");
    assert_eq!(f.end_as(next_step, "TIMEOUT")["case_size"], "bare");
    for id in ["a", "next-step"] {
        let row = f.case_row(id);
        assert_eq!(row["case_detail"], "capture_queue_full");
        assert_eq!(f.case_logs(&row).len(), 1);
    }
    let row = f.case_row("a");
    assert_eq!(
        f.case_logs(&row)[0].additional_fields["coverage"]["omitted_records"]["selector_other"],
        1
    );
    f.close("one");
}

#[test]
fn reader_writing_slot_is_counted_and_reused_after_completion() {
    let f = Fixture::new();
    f.begin("one", 60_000);
    for i in 0..7 {
        assert_eq!(
            f.end_as(f.event(&format!("test-{i}")), "FAIL")["case_size"],
            "full"
        );
    }
    f.with_run(|run| run.take_pending_for_test("test-0", true));
    assert_eq!(
        f.end_as(f.event("overflow"), "FAIL")["case_size"],
        "bare",
        "one slot in writing and six pending"
    );
    f.with_run(|run| {
        run.finish_writer_for_test("test-0");
        run.tick(&f.domain, Instant::now());
    });
    assert_eq!(f.end_as(f.event("reuse"), "FAIL")["case_size"], "full");
}

#[test]
fn reader_eight_deadline_members_share_one_slot_under_pressure() {
    let f = Fixture::new();
    f.begin("one", 3000);
    for i in 0..6 {
        assert_eq!(
            f.end_as(f.event(&format!("pending-{i}")), "FAIL")["case_size"],
            "full"
        );
    }
    for i in 0..8 {
        let mut event = f.event(&format!("deadline-{i}"));
        event["test_timeout_ms"] = json!(250);
        f.rpc("start", event);
    }
    assert_eq!(
        f.domain.test_log.status("tests", None)["test_log"]["executions"]["in_flight"],
        8
    );
    let until = Instant::now() + Duration::from_secs(2);
    let timed_out = loop {
        let rows: Vec<_> = f
            .rows()
            .into_iter()
            .filter(|row| row["event"] == "incomplete")
            .collect();
        if rows.len() == 8 {
            break rows;
        }
        assert!(Instant::now() < until, "eight deadlines");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        timed_out
            .iter()
            .filter(|row| row["case_size"] == "full")
            .count(),
        8
    );
    let mut step = f.event("step");
    step["name"] = json!("step:6-dsl");
    assert_eq!(f.end_as(step, "ABORTED")["case_size"], "full");
    f.close("one");
    let paths: HashMap<_, _> = f
        .rows()
        .into_iter()
        .filter(|row| row["event"] == "case-ready")
        .map(|row| {
            (
                row["execution_id"].as_str().unwrap().to_owned(),
                row["case_path"].clone(),
            )
        })
        .collect();
    for i in 1..8 {
        assert_eq!(paths[&format!("deadline-{i}")], paths["deadline-0"]);
    }
}

#[test]
fn reader_optional_queue_skips_only_charge_the_rerun_attempt() {
    let f = Fixture::new();
    f.rpc(
        "begin",
        json!({"run_id":"one","jsonl_path":f.dir.path().join("one.jsonl"),
        "grace_period_ms":60_000,"max_rerun_cases":1,"max_slow_pass_cases":1}),
    );
    for i in 0..7 {
        f.end_as(f.event(&format!("full-{i}")), "FAIL");
    }
    let slow = |id| {
        let mut event = f.event(id);
        event["started_at"] = json!(Utc::now() - chrono::Duration::seconds(1));
        event["slow_if"] = json!("elapsed > 0ms");
        f.rpc("start", event.clone());
        f.end_as(event, "OK")
    };
    let rerun = |id| {
        let mut event = f.event(id);
        event["rerun_of"] = json!("full-0");
        f.end_as(event, "OK")
    };
    assert_eq!(slow("too-full-slow")["case_status"], "skipped_queue_full");
    assert_eq!(rerun("too-full-rerun")["case_status"], "skipped_queue_full");
    f.with_run(|run| run.take_pending_for_test("full-0", false));
    assert_eq!(rerun("late-rerun")["case_status"], "skipped_rerun_limit");
    assert_eq!(slow("late-slow")["case_status"], "pending");
}

#[test]
fn reader_displaced_slow_pass_has_no_case_size_in_report_input() {
    let f = Fixture::new();
    f.begin("one", 60_000);
    for i in 0..6 {
        f.end_as(f.event(&format!("full-{i}")), "FAIL");
    }
    let mut slow = f.event("slow");
    slow["started_at"] = json!(Utc::now() - chrono::Duration::seconds(1));
    slow["slow_if"] = json!("elapsed > 0ms");
    f.rpc("start", slow.clone());
    assert_eq!(f.end_as(slow, "OK")["case_size"], "full");
    let mut step = f.event("step-slow");
    step["name"] = json!("step:5-rust");
    step["started_at"] = json!(Utc::now() - chrono::Duration::seconds(1));
    step["slow_if"] = json!("elapsed > 0ms");
    f.rpc("start", step.clone());
    assert_eq!(f.end_as(step, "OK")["case_size"], "full");
    assert_eq!(f.end_as(f.event("red"), "FAIL")["case_size"], "full");
    let cutoff = Instant::now() + Duration::from_secs(2);
    loop {
        let rows = f.rows();
        if rows
            .iter()
            .any(|row| row["event"] == "end-test" && row["execution_id"] == "red")
        {
            let row = rows
                .iter()
                .find(|row| row["event"] == "case-skipped" && row["execution_id"] == "slow")
                .unwrap();
            assert_eq!(row["case_status"], "evicted_for_priority");
            assert_eq!(row.get("case_size"), Some(&Value::Null), "{row:?}");
            assert!(!rows.iter().any(|row| {
                row["event"] == "case-skipped" && row["execution_id"] == "step-slow"
            }));
            break;
        }
        assert!(Instant::now() < cutoff);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn reader_late_plan_is_refused_when_run_has_closed() {
    let f = Fixture::new();
    f.begin("one", 0);
    f.close("one");
    let response = f.rpc(
        "begin",
        json!({"run_id":"one",
        "jsonl_path":f.dir.path().join("one.jsonl"),"planned_executions":40}),
    );
    assert_eq!(response["code"], "run_closed");
}

#[test]
fn saturated_capture_pool_keeps_old_evidence_and_writes_a_bare_case() {
    let f = Fixture::new();
    f.domain
        .test_log
        .capture_pool
        .lock()
        .unwrap()
        .set_available(8192);
    f.begin("one", 0);
    let mut a = f.event("a");
    f.rpc("start", a.clone());
    f.log(1, "red A");
    a["verdict"] = json!("FAIL");
    a["ended_at"] = json!(Utc::now());
    assert_eq!(f.rpc("end", a)["case_status"], "pending");
    let mut b = f.event("b");
    f.rpc("start", b.clone());
    f.log(2, &"big B".repeat(2048));
    b["verdict"] = json!("FAIL");
    b["ended_at"] = json!(Utc::now());
    let bare = f.rpc("end", b);
    assert_eq!(bare["case_size"], "bare");
    assert_eq!(bare["case_detail"], "capture_pool_exhausted");
    f.close("one");
    let cases: Vec<_> = f
        .rows()
        .into_iter()
        .filter(|r| r["event"] == "case-ready")
        .collect();
    assert_eq!(cases.len(), 2);
    let a = cases.iter().find(|r| r["execution_id"] == "a").unwrap();
    let path = f.dir.path().join(a["case_path"].as_str().unwrap());
    assert!(crate::cases::load::load(&path)
        .unwrap()
        .logs
        .unwrap()
        .iter()
        .any(|r| r.message == "red A"));
}

#[test]
fn zero_sampling_keeps_failure_and_green_rerun_and_expanded_focus_evidence() {
    let f = Fixture::new();
    f.rpc("begin",json!({"run_id":"one","jsonl_path":f.dir.path().join("one.jsonl"),
        "grace_period_ms":0,"summary_sample_percent":0,"expanded_cases":true,"display_timezone":"Asia/Jerusalem",
        "fields":{"/Build/commit":"abc","/Build/profile":"release","/Action":"test","extra":{"version":3}}}));
    let mut peer = f.event("a");
    f.rpc("start", peer.clone());
    f.log(1, "earlier peer work");
    let mut red = f.event("b");
    f.rpc("start", red.clone());
    f.log(2, "target");
    red["ended_at"] = json!(Utc::now());
    red["verdict"] = json!("FAIL");
    red["fields"] = json!({"failed_test":"member","member_runtime_ms":3});
    f.rpc("end", red);
    peer["ended_at"] = json!(Utc::now());
    peer["verdict"] = json!("OK");
    f.rpc("end", peer);
    let mut rerun = f.event("c");
    rerun["selector"]["value"] = json!(format!("{:032x}", 3));
    rerun["rerun_of"] = json!("b");
    f.rpc("start", rerun.clone());
    f.log(3, "clean");
    rerun["ended_at"] = json!(Utc::now());
    rerun["verdict"] = json!("OK");
    f.rpc("end", rerun);
    f.close("one");
    let rows = f.rows();
    let summary = |id: &str| {
        rows.iter()
            .find(|r| r["event"] == "summary-final" && r["execution_id"] == id)
            .unwrap()
    };
    assert!(summary("a")["summary"].is_null());
    assert_eq!(summary("b")["coverage"]["collection_mode"], "ring_backfill");
    assert_eq!(summary("b")["member_runtime_ms"], 3);
    assert_eq!(summary("c")["coverage"]["collection_mode"], "live");
    for id in ["b", "c"] {
        assert_eq!(summary(id)["summary"]["log_levels"]["INFO"], 1);
    }
    let rel = rows
        .iter()
        .find(|r| r["event"] == "case-ready" && r["execution_id"] == "b")
        .unwrap()["case_path"]
        .as_str()
        .unwrap();
    let stem = format!("member--{:032x}", 2);
    assert_eq!(rel, format!("cases/{stem}/{stem}.md"));
    let path = f.dir.path().join(rel);
    let md = std::fs::read_to_string(&path).unwrap();
    assert!(md.contains("earlier peer work") && md.contains("Asia/Jerusalem"));
    let mut case = crate::cases::load::load(&path).unwrap();
    assert_eq!(case.registry.as_ref().unwrap().facts.len(), 3);
    let logs = case.logs.as_ref().unwrap();
    assert!(logs.iter().any(|l| l.message == "earlier peer work"));
    assert!(logs.iter().any(|l| {
        l.additional_fields
            .get("case_context")
            .is_some_and(|v| v["run_fields"]["extra"]["version"] == 3)
    }));
    case.select_scope("focus", None).unwrap();
    assert!(case
        .logs
        .as_ref()
        .unwrap()
        .iter()
        .all(|e| e.trace_id == Some(2)));
}

#[test]
fn deadline_group_indexes_distinct_foci_and_late_ok_supersedes_incomplete() {
    let f = Fixture::new();
    f.begin("one", 0);
    let mut tests = [f.event("a"), f.event("b")];
    for e in &mut tests {
        e["test_timeout_ms"] = json!(10);
        f.rpc("start", e.clone());
    }
    f.log(1, "original A");
    f.log(2, "original B");
    let limit = Instant::now() + Duration::from_secs(2);
    while f
        .rows()
        .iter()
        .filter(|r| r["event"] == "incomplete")
        .count()
        < 2
    {
        assert!(Instant::now() < limit);
        std::thread::sleep(Duration::from_millis(5));
    }
    for mut e in tests {
        e["ended_at"] = json!(Utc::now());
        e["verdict"] = json!("OK");
        f.rpc("end", e);
    }
    // Advance the maintenance clock past the first group's fixed window.
    // A subsequent crossing must create an independently loadable case.
    f.domain
        .test_log
        .state
        .lock()
        .unwrap()
        .run
        .as_mut()
        .unwrap()
        .tick(&f.domain, Instant::now() + Duration::from_secs(61));
    let mut third = f.event("c");
    third["selector"]["value"] = json!(format!("{:032x}", 3));
    third["test_timeout_ms"] = json!(2);
    f.rpc("start", third.clone());
    let limit = Instant::now() + Duration::from_secs(2);
    while !f
        .rows()
        .iter()
        .any(|r| r["event"] == "incomplete" && r["execution_id"] == "c")
    {
        assert!(Instant::now() < limit);
        std::thread::sleep(Duration::from_millis(5));
    }
    third["ended_at"] = json!(Utc::now());
    third["verdict"] = json!("OK");
    f.rpc("end", third);
    f.close("one");
    let v = f.domain.test_log.status("tests", Some("one"))["test_log"].clone();
    assert_eq!(v["cases"]["ready_executions"], 3);
    assert_eq!(v["cases"]["distinct_cases_written"], 2);
    assert_eq!(v["cases"]["files"].as_array().unwrap().len(), 2);
    let rows = f.rows();
    let path = |id: &str| {
        rows.iter()
            .find(|r| r["event"] == "case-ready" && r["execution_id"] == id)
            .unwrap()["case_path"]
            .as_str()
            .unwrap()
    };
    assert_eq!(path("a"), path("b"));
    assert_ne!(path("a"), path("c"));
    for r in rows.iter().filter(|r| r["event"] == "summary-final") {
        assert_eq!(r["verdict"], "OK");
        assert_eq!(r["closure_incomplete"], false);
        assert_eq!(r["supersedes_incomplete"], true);
    }
    let mut case = crate::cases::load::load(&f.dir.path().join(path("a"))).unwrap();
    assert_eq!(case.front.focus_traces.len(), 2);
    assert!(case.select_scope("focus", None).is_err());
    case.select_scope("focus", Some("2")).unwrap();
    assert!(case.logs.unwrap().iter().any(|e| e.message == "original B"));
}

#[test]
fn bounded_rings_grow_only_for_evidence_in_active_and_grace_windows() {
    let f = Fixture::new();
    let logs = &f.domain.pipeline.store;
    let spans = &f.domain.span_store;
    logs.resize_with(Some(2), Some(4), || spans.resize(Some(2), Some(6)));
    let span = || {
        let at = Utc::now();
        let span = serde_json::from_value(json!({"seq":0,"trace_id":"00000000000000000000000000000001","span_id":"0000000000000001","parent_span_id":null,"start_time":at,"end_time":at,"duration_ms":0,"name":"work","kind":"internal","service_name":"worker","status":{"type":"ok"},"attributes":{},"events":[]})).unwrap();
        spans.insert(span);
    };
    for _ in 0..5 {
        f.log(2, "before");
        span();
    }
    assert_eq!(
        (logs.ring_info().0.current, spans.ring_info().0.current),
        (2, 2)
    );
    f.begin("one", 120);
    let mut event = f.event("a");
    f.rpc("start", event.clone());
    for _ in 0..2 {
        f.log(2, "peer");
        span();
    }
    assert_eq!(
        (logs.ring_info().0.current, spans.ring_info().0.current),
        (2, 2)
    );
    f.log(2, "peer");
    assert_eq!(
        (logs.ring_info().0.current, spans.ring_info().0.current),
        (4, 2)
    );
    event["ended_at"] = json!(Utc::now());
    event["verdict"] = json!("OK");
    f.rpc("end", event);
    span();
    assert_eq!(spans.ring_info().0.current, 4);
    for _ in 0..8 {
        f.log(2, "peer");
    }
    assert_eq!(logs.ring_info().0.current, 4);
    assert!(f.domain.pipeline.lost_below() > 0);
    f.close("one");
    for _ in 0..10 {
        span();
    }
    assert_eq!(spans.ring_info().0.current, 4);
}

#[test]
fn progress_counts_reconstruction_replay_caps_and_history_without_writer_lock() {
    let f = Fixture::new();
    let log = &f.domain.test_log;
    assert!(log.status("lane", None)["test_log"].is_null());
    assert_eq!(log.list("lane")["runs"], json!([]));
    f.rpc(
        "begin",
        json!({"run_id":"one","jsonl_path":f.dir.path().join("one.jsonl"),"planned_executions":24,
        "fields":{"/Action":"merge-check"},"max_failures":0,"grace_period_ms":1}),
    );
    for i in 0..24 {
        let mut e = f.event(&i.to_string());
        if i % 2 == 0 {
            f.rpc("start", e.clone());
        }
        e["ended_at"] = json!(Utc::now());
        e["verdict"] = json!("FAIL");
        e["fields"] = json!({"message":"assertion","rerun_command":"rerun"});
        let batch = json!({"run_id":"one","events":[e.clone(),e]});
        let result = f.rpc("replay", batch);
        assert_eq!(result["results"][1]["status"], "already_applied");
    }
    // Reading remains possible while the lifecycle mutex is held, as during a slow capture.
    let locked = log.state.lock().unwrap();
    let v = log.status("lane", None)["test_log"].clone();
    let _: logmon_broker_protocol::TestLogRunDetail = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(
        v["executions"],
        json!({"planned":24,"started":24,"ended":24,"in_flight":0,
        "by_verdict":{"OK":0,"FAIL":24,"TIMEOUT":0,"ABORTED":0,"NO_VERDICT":0}})
    );
    assert_eq!(v["replayed_ends"], 24);
    assert_eq!(v["cases"]["requested"], 24);
    assert_eq!(v["cases"]["skipped"], 0);
    assert_eq!(v["cases"]["refused_by_max_failures"], 0);
    assert!(v["cases"]["minimal_by_failure_limit"].as_u64().unwrap() > 0);
    assert_eq!(v["recent_failures"].as_array().unwrap().len(), 20);
    assert_eq!(v["recent_failures"][0]["execution_id"], "23");
    assert!(matches!(
        v["recent_failures"][0]["case_size"].as_str(),
        Some("minimal" | "bare")
    ));
    assert_eq!(v["recent_failures_truncated"], true);
    assert_eq!(log.list("lane")["runs"][0]["cases"].get("files"), None);
    assert_eq!(log.projection().unwrap().in_flight, 0);
    drop(locked);
    f.close("one");
    assert!(log.status("lane", None)["test_log"].is_null());
    let settled = f
        .rows()
        .into_iter()
        .find(|r| r["event"] == "run-settled")
        .unwrap();
    let v = log.status("lane", Some("one"))["test_log"].clone();
    assert_eq!(settled["aggregates"]["executions"], v["executions"]);
    assert_eq!(v["summaries"], json!({"pending":0,"final":24}));
    assert_eq!(v["state"], "closed");
    for i in 0..9 {
        let id = format!("run-{i}");
        f.begin(&id, 0);
        f.close(&id);
    }
    f.begin("successor", 0);
    assert_eq!(log.list("lane")["runs"].as_array().unwrap().len(), 11);
    f.rpc("seal", json!({"run_id":"successor","wait_timeout_ms":0}));
    f.close("successor");
    assert_eq!(log.list("lane")["runs"].as_array().unwrap().len(), 10);
    assert_eq!(
        log.status("lane", Some("one"))["error"]["code"],
        "run_not_found"
    );
    assert_eq!(
        log.status("lane", Some("successor"))["test_log"]["state"],
        "sealed"
    );
    assert!(f.dir.path().join("one.jsonl").exists());
}

#[test]
fn repeated_begin_sets_an_unknown_plan_once() {
    let f = Fixture::new();
    let log = &f.domain.test_log;
    let mut begin = json!({
        "run_id":"one","jsonl_path":f.dir.path().join("one.jsonl"),
        "grace_period_ms":0,"max_failures":9
    });
    assert_eq!(f.rpc("begin", begin.clone())["status"], "accepted");
    let mut execution = f.event("a");
    f.rpc("start", execution.clone());
    assert!(log.status("tests", None)["test_log"]["executions"]["planned"].is_null());

    begin["planned_executions"] = json!(40);
    let unused_path = f.dir.path().join("unused.jsonl");
    begin["jsonl_path"] = json!(unused_path);
    begin["max_failures"] = json!(1);
    assert_eq!(f.rpc("begin", begin.clone())["status"], "accepted");
    assert_eq!(f.rpc("begin", begin.clone())["status"], "already_applied");
    begin["planned_executions"] = json!(41);
    let conflict = f.rpc("begin", begin);
    assert_eq!(conflict["status"], "refused");
    assert_eq!(conflict["code"], "planned_executions_conflict");

    let detail = log.status("tests", None)["test_log"].clone();
    assert_eq!(detail["executions"]["planned"], 40);
    assert_eq!(detail["executions"]["started"], 1);
    assert_eq!(detail["cases"]["max_failures"], 9);
    assert_eq!(log.list("tests")["runs"][0]["executions"]["planned"], 40);

    execution["verdict"] = json!("OK");
    execution["ended_at"] = json!(Utc::now());
    assert_eq!(f.rpc("end", execution)["status"], "accepted");
    f.close("one");
    assert!(!unused_path.exists());
    let rows = f.rows();
    assert_eq!(
        rows.iter()
            .filter(|r| r["event"] == "begin-test-log")
            .count(),
        1
    );
    let updates: Vec<_> = rows
        .iter()
        .filter(|r| r["event"] == "test-log-plan")
        .collect();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["planned_executions"], 40);
    let settled = rows.iter().find(|r| r["event"] == "run-settled").unwrap();
    assert_eq!(settled["aggregates"]["executions"]["planned"], 40);
    assert_eq!(
        log.status("tests", Some("one"))["test_log"]["executions"]["planned"],
        40
    );
}

#[test]
fn progress_lists_oldest_three_and_freezes_at_settlement() {
    let f = Fixture::new();
    f.rpc(
        "begin",
        json!({"run_id":"one","jsonl_path":f.dir.path().join("one.jsonl"),
      "planned_executions":0,"grace_period_ms":100}),
    );
    let base = Utc::now() - chrono::Duration::seconds(15);
    for i in [2, 0, 3, 1] {
        let mut e = f.event(&format!("test-{i}"));
        e["started_at"] = json!(base + chrono::Duration::seconds(i));
        e["selector"] = json!({"field":"worker","value":format!("worker-{i}")});
        f.rpc("start", e);
    }
    let log = &f.domain.test_log;
    let detail = log.status("tests", None)["test_log"].clone();
    assert_eq!(detail["executions"]["planned"], 0);
    assert!(detail["last_completion_at"].is_null());
    assert_eq!(detail["executions"]["in_flight"], 4);
    let oldest = detail["longest_in_flight"].as_array().unwrap();
    let ids: Vec<_> = oldest
        .iter()
        .map(|x| x["execution_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["test-0", "test-1", "test-2"]);
    assert!(oldest
        .iter()
        .all(|x| x["trace_id"].is_null() && x["deadline_overdue_ms"] == 0));
    f.rpc("seal", json!({"run_id":"one","wait_timeout_ms":0}));
    assert_eq!(log.projection().unwrap().state, TestLogRunState::Closing);
    f.close("one");
    let detail = log.status("tests", Some("one"))["test_log"].clone();
    assert_eq!(detail["executions"]["by_verdict"]["NO_VERDICT"], 4);
    assert_eq!(detail["longest_in_flight"], json!([]));
    std::thread::sleep(Duration::from_millis(3));
    assert_eq!(
        detail["elapsed_ms"],
        log.status("tests", Some("one"))["test_log"]["elapsed_ms"]
    );
}

#[test]
fn machine_series_brackets_focus_without_displacing_execution_evidence() {
    let f = Fixture::with_capacity(1);
    f.rpc("begin", json!({"run_id":"one","jsonl_path":f.dir.path().join("one.jsonl"),"expanded_cases":true,"grace_period_ms":0}));
    let mut execution = f.event("a");
    let origin = Utc::now() - chrono::Duration::minutes(1);
    let ts = |n| origin + chrono::Duration::seconds(n);
    execution["started_at"] = json!(ts(10));
    f.rpc("start", execution.clone());
    f.log(1, "must survive");
    for n in [21, 0, 9, 20, 10, 22] {
        assert_eq!(f.rpc("machine_load",json!({"run_id":"one","ts":ts(n),"host":"test","kind":"recorder","fields":{"sequence":n}}))["status"],"accepted");
    }
    execution["ended_at"] = json!(ts(20));
    execution["verdict"] = json!("FAIL");
    f.rpc("end", execution);
    f.close("one");
    let ready = f.case_row("a");
    let report =
        std::fs::read_to_string(f.dir.path().join(ready["case_path"].as_str().unwrap())).unwrap();
    assert!(report.contains("Machine samples (4)"));
    assert!(report.contains("test recorder sequence=9"));
    assert!(!report.contains("sequence=22"));
    let logs = f.case_logs(&ready);
    assert!(logs.iter().any(|log| log.message == "must survive"));
    let mut values: Vec<_> = logs
        .iter()
        .filter(|r| r.message == "machine-load")
        .map(|r| {
            assert_eq!(r.additional_fields["kind"], "recorder");
            r.additional_fields["sample"]["sequence"].as_i64().unwrap()
        })
        .collect();
    values.sort();
    assert_eq!(values, [9, 10, 20, 21]);
}

#[test]
fn machine_sample_capacity_refuses_explicitly() {
    let f = Fixture::new();
    f.begin("one", 0);
    let row = json!({"run_id":"one","ts":Utc::now(),"host":"a","fields":{"s":"x".repeat(8 << 20)}});
    assert_eq!(
        f.rpc("machine_load", row)["code"],
        "machine_sample_capacity"
    );
    assert_eq!(
        f.rpc(
            "machine_load",
            json!({"run_id":"one","ts":Utc::now(),"host":"a"})
        )["status"],
        "accepted"
    );
    f.close("one");
    assert!(f
        .rows()
        .iter()
        .any(|r| r["event"] == "machine-load" && r["kind"] == "machine-load"));
}
