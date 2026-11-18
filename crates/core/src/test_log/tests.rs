use super::*;
use crate::daemon::domain::{DomainConfig, DomainId, DomainSource};
use crate::gelf::message::{Level, LogEntry};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    domain: Arc<Domain>,
}
impl Fixture {
    fn new() -> Self {
        let domain = Arc::new(Domain::new(
            DomainConfig {
                name: DomainId::new("tests").unwrap(),
                gelf_port: 0,
                otlp_grpc_port: 0,
                otlp_http_port: 0,
                log_buffer_size: 20,
                span_buffer_size: 20,
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
        let mut e = LogEntry::synthetic(Level::Info, msg);
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
        json!({"run_id":"one","jsonl_path":path,"max_failures":0,"grace_period_ms":0}),
    );
    let mut a = f.event("a");
    f.log(1, "already stored");
    a["ended_at"] = json!(Utc::now());
    a["verdict"] = json!("FAIL");
    let result = f.rpc("replay", json!({"run_id":"one","events":[a.clone(),a]}));
    assert_eq!(result["results"][0]["status"], "applied");
    assert_eq!(result["results"][0]["case_status"], "skipped_failure_limit");
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
    for i in 0..8 {
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
    assert_eq!(
        attempt("another", "FAIL", None)["case_status"],
        "skipped_failure_limit"
    );
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
    assert_eq!(pending["pending_cases"].as_array().unwrap().len(), 8);
    let ids: Vec<_> = pending["pending_cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["execution_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"red") && ids.contains(&"isolated"));
    assert!(!ids.contains(&"ok-7") && !ids.contains(&"ok-6"));
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
    a["verdict"] = json!("OK");
    a["ended_at"] = json!(Utc::now());
    f.rpc("end", a);
    f.close("one");
    let rows = f.rows();
    assert_eq!(
        rows.iter().filter(|v| v["event"] == "case-ready").count(),
        1
    );
    assert!(
        rows.iter()
            .any(|v| v["event"] == "end-test" && v["verdict"] == "OK")
    );
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
