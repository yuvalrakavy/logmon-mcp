//! The `collectors.*` and `traces.profile` contract surface (spec §7), plus the
//! §1.1 `traces.slow` repair.
//!
//! Everything goes through `RpcHandler::handle`, so these exercise the wire
//! shapes a client actually sees — parameter parsing, admission, error text —
//! rather than the projection layer, which has its own unit tests.

use logmon_broker_core::collector::state::DEFAULT_MAX_SAMPLE_BYTES;
use logmon_broker_core::daemon::domain::{
    Domain, DomainConfig, DomainId, DomainRegistry, DomainSource,
};
use logmon_broker_core::daemon::rpc_handler::{DomainPolicy, RpcHandler};
use logmon_broker_core::daemon::session::{SessionId, SessionRegistry};
use logmon_broker_core::daemon::span_processor::process_span_for_domain;
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use logmon_broker_core::receiver::ReceiverMetrics;
use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::span::types::{SpanEntry, SpanKind, SpanStatus};
use logmon_broker_core::store::bookmarks::BookmarkStore;
use logmon_broker_protocol::RpcRequest;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

struct Harness {
    handler: Arc<RpcHandler>,
    domains: Arc<DomainRegistry>,
    sessions: Arc<SessionRegistry>,
    collectors: Arc<logmon_broker_core::collector::registry::CollectorRegistry>,
    pipeline: Arc<LogPipeline>,
}

fn make_domain(name: &str) -> Arc<Domain> {
    let seq = Arc::new(SeqCounter::new());
    Arc::new(Domain::from_parts(
        DomainConfig {
            name: DomainId::new(name).unwrap_or_else(|_| DomainId::default_domain()),
            gelf_port: 0,
            otlp_grpc_port: 0,
            otlp_http_port: 0,
            log_buffer_size: 1000,
            span_buffer_size: 1000,
            source: DomainSource::Config,
        },
        Arc::new(LogPipeline::new_with_seq_counter(1000, seq.clone())),
        Arc::new(SpanStore::new(1000, seq)),
        Arc::new(BookmarkStore::new()),
        Arc::new(ReceiverMetrics::new()),
    ))
}

fn harness() -> Harness {
    harness_in(None)
}

/// A harness whose collectors persist into `dir`.
///
/// `None` disables persistence, which is what almost every test wants: a test
/// that persisted would write into whatever directory it was handed, and the
/// live daemon's is one wrong argument away.
fn harness_in(dir: Option<std::path::PathBuf>) -> Harness {
    let domains = Arc::new(DomainRegistry::new());
    let default = make_domain("default");
    let pipeline = default.pipeline.clone();
    domains.insert(default);
    let sessions = Arc::new(SessionRegistry::new());
    let registry = logmon_broker_core::collector::registry::CollectorRegistry::new();
    let collectors = Arc::new(match dir {
        Some(d) => registry.with_persistence(d),
        None => registry,
    });
    let handler = Arc::new(RpcHandler::new(
        domains.clone(),
        sessions.clone(),
        collectors.clone(),
        vec!["test".into()],
        DomainPolicy {
            max_domains: 32,
            default_log_buffer_size: 1000,
            default_span_buffer_size: 1000,
            stale_after_secs: 60,
        },
    ));
    Harness {
        handler,
        domains,
        sessions,
        collectors,
        pipeline,
    }
}

impl Harness {
    fn call(&self, sid: &SessionId, method: &str, params: Value) -> Result<Value, String> {
        let req = RpcRequest::new(1, method, params);
        let resp = self.handler.handle(sid, &req);
        match resp.error {
            Some(e) => Err(e.message),
            None => Ok(resp.result.unwrap_or(Value::Null)),
        }
    }

    /// Feed a span through the real ingest path for `domain`, so collectors see
    /// it exactly as they would in the daemon.
    fn feed(&self, domain: &str, span: &SpanEntry) {
        let id = DomainId::new(domain).expect("valid domain name");
        let d = self.domains.get(&id).expect("domain exists");
        process_span_for_domain(
            span,
            &d.span_store,
            &self.sessions,
            &self.pipeline,
            &self.collectors,
            &id,
        );
    }
}

fn span(service: &str, name: &str, ms: f64) -> SpanEntry {
    span_at(service, name, ms, 0)
}

fn span_at(service: &str, name: &str, ms: f64, start_offset_ns: i64) -> SpanEntry {
    let base = chrono::DateTime::from_timestamp_nanos(1_700_000_000_000_000_000 + start_offset_ns);
    SpanEntry {
        seq: 0,
        trace_id: 7,
        span_id: 1,
        parent_span_id: None,
        start_time: base,
        end_time: base + chrono::Duration::nanoseconds((ms * 1_000_000.0) as i64),
        duration_ms: ms,
        name: name.into(),
        kind: SpanKind::Internal,
        service_name: service.into(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[test]
fn a_collector_round_trips_through_add_list_get_reset_and_remove() {
    let h = harness();
    let sid = h.sessions.create_named("A").expect("session");

    let added = h
        .call(
            &sid,
            "collectors.add",
            json!({
                "name": "cache-ab",
                "filter": "sv=store_server",
                "level": "tree",
                "description": "measuring the read-through cache",
            }),
        )
        .expect("armed");
    assert_eq!(added["name"], "cache-ab");
    assert_eq!(added["level"], "tree");
    assert_eq!(added["domain"], "default");

    for i in 0..5 {
        h.feed(
            "default",
            &span_at("store_server", "reconcile", 10.0, i * 20_000_000),
        );
    }
    h.feed("default", &span("other_service", "ignored", 500.0));

    let listed = h.call(&sid, "collectors.list", json!({})).expect("listed");
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["collectors"][0]["matched"], 5, "only matching spans");
    assert_eq!(
        listed["collectors"][0]["description"], "measuring the read-through cache",
        "the reason is carried, so a number found later still has context"
    );

    let got = h
        .call(&sid, "collectors.get", json!({ "name": "cache-ab" }))
        .expect("read");
    assert_eq!(got["matched"], 5);
    assert_eq!(got["exact"]["count"], 5);
    assert_eq!(got["exact"]["total_ms"], 50.0);
    assert_eq!(got["exact"]["avg_ms"], 10.0);
    assert_eq!(got["sampled"]["sample_count"], 5);
    assert_eq!(got["sampled"]["complete"], true);
    assert!(got["window"]["wall_ms"].as_f64().is_some());

    let reset = h
        .call(&sid, "collectors.reset", json!({ "name": "cache-ab" }))
        .expect("reset");
    assert_eq!(
        reset["discarded"]["matched"], 5,
        "a reset one call too early is otherwise unrecoverable"
    );
    assert_eq!(reset["discarded"]["total_ms"], 50.0);

    let after = h
        .call(&sid, "collectors.get", json!({ "name": "cache-ab" }))
        .expect("read");
    assert_eq!(after["matched"], 0, "and the collector starts clean");
    assert!(after["window"]["zeroed_at"].is_string());

    h.call(&sid, "collectors.remove", json!({ "name": "cache-ab" }))
        .expect("removed");
    assert_eq!(
        h.call(&sid, "collectors.list", json!({})).unwrap()["count"],
        0
    );
    assert!(h
        .call(&sid, "collectors.get", json!({ "name": "cache-ab" }))
        .unwrap_err()
        .contains("no collector named"));
}

#[test]
fn collectors_are_scoped_to_the_session_that_armed_them() {
    let h = harness();
    let a = h.sessions.create_named("A").unwrap();
    let b = h.sessions.create_named("B").unwrap();

    h.call(
        &a,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .expect("armed");
    assert_eq!(
        h.call(&b, "collectors.list", json!({})).unwrap()["count"],
        0
    );
    // The same name in another session is a different collector, not a clash.
    h.call(
        &b,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .expect("a name is unique per session, not per daemon");
    assert!(h
        .call(
            &a,
            "collectors.add",
            json!({ "name": "c", "filter": "ALL" })
        )
        .unwrap_err()
        .contains("already exists"));
}

#[test]
fn a_collector_keeps_following_its_pinned_domain_after_the_owner_rebinds() {
    // §4.4: ownership and pinning are different questions. Reaching collectors
    // through the session's *current* binding would silently stop this one
    // while it still reported the domain it was armed in.
    let h = harness();
    h.domains.insert(make_domain("t3"));
    h.domains.insert(make_domain("t9"));
    let sid = h.sessions.create_named("A").unwrap();

    // Armed while bound to t3 — deliberately NOT the default domain, so the
    // test distinguishes "pinned to where it was armed" from "pinned to
    // whatever domain happens to be first".
    h.call(&sid, "domains.use", json!({ "name": "t3" }))
        .unwrap();
    let added = h
        .call(
            &sid,
            "collectors.add",
            json!({ "name": "c", "filter": "ALL" }),
        )
        .expect("armed in t3");
    assert_eq!(added["domain"], "t3", "and it reports where it is pinned");

    h.call(&sid, "domains.use", json!({ "name": "t9" }))
        .expect("rebound");

    h.feed("t3", &span("svc", "still-collected", 5.0));
    h.feed("t9", &span("svc", "not-mine", 5.0));
    h.feed("default", &span("svc", "also-not-mine", 5.0));

    let got = h
        .call(&sid, "collectors.get", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(
        got["matched"], 1,
        "the pin held: it did not follow the owner, and it is not the default"
    );
}

#[test]
fn dropping_a_session_hands_back_its_sample_reservation() {
    // The reservation is daemon-wide and fits about four default-sized
    // collectors. A dropped session that kept them would take that budget with
    // it, and nothing could be armed again until the daemon restarted.
    let h = harness();
    let sid = h.sessions.create_named("doomed").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .expect("armed");
    assert!(h.collectors.reserved_bytes() > 0);

    // `session.drop` refuses a connected session, and a named session keeps its
    // collectors across a disconnect on purpose — that IS the arm, run, read
    // workflow. So the disconnect first, and the budget must survive it.
    h.sessions.disconnect(&sid);
    assert!(
        h.collectors.reserved_bytes() > 0,
        "a disconnect alone must not discard an armed collector"
    );

    let dropped = h
        .call(&sid, "session.drop", json!({ "name": "doomed" }))
        .expect("dropped");
    assert_eq!(dropped["collectors_released"], 1, "and it says how many");
    assert_eq!(
        h.collectors.reserved_bytes(),
        0,
        "the budget is available again"
    );
}

#[test]
fn clearing_a_sessions_collectors_leaves_every_other_session_alone() {
    let h = harness();
    let a = h.sessions.create_named("A").unwrap();
    let b = h.sessions.create_named("B").unwrap();
    h.call(
        &a,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    h.call(
        &b,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();

    assert_eq!(h.handler.clear_session_collectors(&a), 1);
    assert_eq!(
        h.call(&b, "collectors.list", json!({})).unwrap()["count"],
        1,
        "B's collector survives A's disposal"
    );
    assert_eq!(
        h.call(&a, "collectors.list", json!({})).unwrap()["count"],
        0
    );
    // Idempotent: a TTL sweep can race an explicit drop.
    assert_eq!(h.handler.clear_session_collectors(&a), 0);
}

// ---------------------------------------------------------------------------
// Persistence and restart (§10, V11)
// ---------------------------------------------------------------------------

/// A second daemon over the same directory. Deliberately **no shutdown** on the
/// first one — §12 notes that a graceful `restart()` would pass V11 regardless,
/// so the point is to prove the files were already complete while the first
/// daemon was still running.
fn restart_over(dir: &std::path::Path) -> Harness {
    let h = harness_in(Some(dir.to_path_buf()));
    let report = h.collectors.restore(|_| Arc::new(ReceiverMetrics::new()));
    assert!(
        report.quarantined.is_empty() && report.rejected.is_empty(),
        "restore had problems: {:?} {:?}",
        report.quarantined,
        report.rejected
    );
    h
}

#[test]
fn v11_a_definition_is_on_disk_before_anything_is_shut_down() {
    // The defect: writing definitions at graceful shutdown means a kill -9
    // loses them, leaving snapshots with nothing to attach to. Asserted
    // mid-life rather than across a shutdown, which is a stricter test than
    // restarting cleanly would be.
    let d = tempfile::TempDir::new().unwrap();
    let h = harness_in(Some(d.path().to_path_buf()));
    let sid = h.sessions.create_named("perf").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "sv=svc", "level": "tree" }),
    )
    .expect("armed");

    let path = logmon_broker_core::collector::persist::collector_path(d.path(), "perf", "c");
    assert!(path.exists(), "the definition reached disk at arm time");
}

#[test]
fn v11_definition_and_history_survive_a_restart_and_the_window_does_not() {
    let d = tempfile::TempDir::new().unwrap();
    let h = harness_in(Some(d.path().to_path_buf()));
    let sid = h.sessions.create_named("perf").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({
            "name": "c", "filter": "sv=svc", "level": "tree",
            "group_keys": ["cache.enabled"],
            "description": "the read-through cache",
        }),
    )
    .unwrap();

    for i in 0..4 {
        h.feed("default", &span_at("svc", "op", 25.0, i * 100_000_000));
    }
    h.call(
        &sid,
        "collectors.snapshot",
        json!({ "name": "c", "label": "baseline", "description": "before" }),
    )
    .unwrap();
    // A second window with live, unsnapshotted data — this is what must NOT
    // come back.
    h.feed("default", &span("svc", "op", 999.0));

    let after = restart_over(d.path());
    let sid2 = after.sessions.create_named("perf").unwrap();

    let listed = after
        .call(&sid2, "collectors.list", json!({}))
        .expect("listed");
    assert_eq!(listed["count"], 1, "the collector came back armed");
    let c = &listed["collectors"][0];
    assert_eq!(c["filter"], "sv=svc");
    assert_eq!(c["level"], "tree");
    assert_eq!(c["group_keys"][0], "cache.enabled");
    assert_eq!(c["description"], "the read-through cache");
    assert_eq!(c["snapshots"], 1);
    // Zeroed, and it says why — so a caller seeing 0 can tell "no traffic yet"
    // from "the run you were measuring did not survive".
    assert_eq!(c["matched"], 0);
    assert_eq!(c["zeroed_by"], "daemon_restart");

    // The recorded run is intact, with its own definition and description.
    let hist = after
        .call(&sid2, "collectors.history", json!({ "name": "c" }))
        .expect("history");
    assert_eq!(hist["count"], 1);
    assert_eq!(hist["snapshots"][0]["label"], "baseline");
    assert_eq!(hist["snapshots"][0]["description"], "before");
    assert_eq!(hist["snapshots"][0]["exact"]["total_ms"], 100.0);
    assert_eq!(hist["snapshots"][0]["filter"], "sv=svc");
    // The sketch survived, so estimated percentiles still work.
    assert!(hist["snapshots"][0]["estimated"]["p50_ms"].is_number());
}

#[test]
fn a_restored_collector_still_collects() {
    // Armed, not merely remembered.
    let d = tempfile::TempDir::new().unwrap();
    {
        let h = harness_in(Some(d.path().to_path_buf()));
        let sid = h.sessions.create_named("perf").unwrap();
        h.call(
            &sid,
            "collectors.add",
            json!({ "name": "c", "filter": "sv=svc" }),
        )
        .unwrap();
    }
    let after = restart_over(d.path());
    let sid = after.sessions.create_named("perf").unwrap();
    after.feed("default", &span("svc", "op", 12.0));

    let got = after
        .call(&sid, "collectors.get", json!({ "name": "c" }))
        .expect("read");
    assert_eq!(got["matched"], 1, "it is collecting, not just listed");
    assert_eq!(got["exact"]["total_ms"], 12.0);
}

#[test]
fn a_collector_pinned_to_a_vanished_domain_is_reported_orphaned_with_the_remedy() {
    // §10: ephemeral domains are never re-created, so for a collector armed on
    // one this is the NORMAL outcome of a restart, not an exception.
    let d = tempfile::TempDir::new().unwrap();
    {
        let h = harness_in(Some(d.path().to_path_buf()));
        h.domains.insert(make_domain("t9"));
        let sid = h.sessions.create_named("perf").unwrap();
        h.call(&sid, "domains.use", json!({ "name": "t9" }))
            .unwrap();
        h.call(
            &sid,
            "collectors.add",
            json!({ "name": "c", "filter": "ALL" }),
        )
        .unwrap();
    }
    // The restart brings back `default` only.
    let after = restart_over(d.path());
    let sid = after.sessions.create_named("perf").unwrap();
    let listed = after.call(&sid, "collectors.list", json!({})).unwrap();
    let c = &listed["collectors"][0];
    assert_eq!(c["domain"], "t9");
    assert_eq!(c["orphaned"], true);
    let note = c["orphan_note"].as_str().expect("a note");
    assert!(note.contains("collectors.edit"), "and the remedy: {note}");

    // And the remedy works.
    after
        .call(
            &sid,
            "collectors.edit",
            json!({ "name": "c", "domain": "default" }),
        )
        .expect("re-pinned");
    after.feed("default", &span("svc", "op", 5.0));
    assert_eq!(
        after
            .call(&sid, "collectors.get", json!({ "name": "c" }))
            .unwrap()["matched"],
        1,
        "collecting again on the new pin"
    );
}

#[test]
fn removing_a_collector_deletes_its_file() {
    let d = tempfile::TempDir::new().unwrap();
    let h = harness_in(Some(d.path().to_path_buf()));
    let sid = h.sessions.create_named("perf").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    let path = logmon_broker_core::collector::persist::collector_path(d.path(), "perf", "c");
    assert!(path.exists());

    h.call(&sid, "collectors.remove", json!({ "name": "c" }))
        .unwrap();
    assert!(
        !path.exists(),
        "otherwise a daemon whose sessions are swept daily leaks files forever"
    );
    assert!(restart_over(d.path())
        .call(
            &h.sessions.create_named("perf2").unwrap(),
            "collectors.list",
            json!({})
        )
        .unwrap()["collectors"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn an_anonymous_session_cannot_arm_a_collector() {
    // §10/§4.4: its name is a UUID that will never be presented again, so the
    // collector would be unreachable the moment it disconnected while still
    // holding a slice of the daemon-wide reservation.
    let h = harness();
    let sid = h.sessions.create_anonymous();
    let err = h
        .call(
            &sid,
            "collectors.add",
            json!({ "name": "c", "filter": "ALL" }),
        )
        .unwrap_err();
    assert!(err.contains("named session"), "got: {err}");
    assert!(err.contains("traces.profile"), "and the alternative");
}

// ---------------------------------------------------------------------------
// collectors.edit (§7.1)
// ---------------------------------------------------------------------------

#[test]
fn a13_a_structural_edit_zeroes_the_window_and_keeps_the_history() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "sv=svc", "level": "tree" }),
    )
    .unwrap();
    h.feed("default", &span("svc", "op", 10.0));
    h.call(
        &sid,
        "collectors.snapshot",
        json!({ "name": "c", "label": "before-edit" }),
    )
    .unwrap();
    h.feed("default", &span("svc", "op", 20.0));

    let edited = h
        .call(
            &sid,
            "collectors.edit",
            json!({ "name": "c", "filter": "sn=op", "level": "timing" }),
        )
        .expect("edited");
    assert_eq!(edited["zeroed"], true);
    assert_eq!(edited["filter"], "sn=op");
    assert_eq!(edited["level"], "timing");

    assert_eq!(
        h.call(&sid, "collectors.get", json!({ "name": "c" }))
            .unwrap()["matched"],
        0,
        "the live window went"
    );
    let hist = h
        .call(&sid, "collectors.history", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(hist["count"], 1, "history did not");
    assert_eq!(
        hist["snapshots"][0]["filter"], "sv=svc",
        "and the snapshot still reports the definition IT was taken under"
    );
}

#[test]
fn editing_only_the_description_discards_nothing() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "sv=svc" }),
    )
    .unwrap();
    h.feed("default", &span("svc", "op", 10.0));

    let edited = h
        .call(
            &sid,
            "collectors.edit",
            json!({ "name": "c", "description": "now with context" }),
        )
        .expect("edited");
    assert_eq!(edited["zeroed"], false);
    assert_eq!(edited["description"], "now with context");
    let got = h
        .call(&sid, "collectors.get", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(got["matched"], 1, "a rename must not cost a measurement");
    // And not only the exact tier. Rebuilding the inner state around a new
    // definition is easy to get half-right: keep the counts, lose the samples,
    // and the result is `sample_count < count` with `complete: true` — the one
    // reconciliation rule this design has, broken by a rename.
    assert_eq!(got["sampled"]["sample_count"], 1);
    assert_eq!(got["sampled"]["complete"], true);
}

#[test]
fn a_structural_edit_that_would_exceed_the_budget_is_refused() {
    // The code comment names three historical ways an edit walked past the
    // reservation — a free max_sample_bytes raise among them — and mutation
    // testing found the re-check itself had no test. Deleting it stayed green.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    // Two collectors, each holding a quarter of the daemon-wide reservation.
    for name in ["a", "b"] {
        h.call(
            &sid,
            "collectors.add",
            json!({ "name": name, "filter": "ALL", "max_sample_bytes": 64 * 1024 * 1024 }),
        )
        .expect("armed");
    }
    // Raising one to the whole budget cannot fit beside the other.
    let err = h
        .call(
            &sid,
            "collectors.edit",
            json!({ "name": "a", "max_sample_bytes": 256 * 1024 * 1024u64 }),
        )
        .unwrap_err();
    assert!(err.contains("remain"), "got: {err}");

    // And the refusal changed nothing — the old size is still in force.
    let listed = h.call(&sid, "collectors.list", json!({})).unwrap();
    assert_eq!(
        listed["reserved_bytes"],
        (128 * 1024 * 1024u64),
        "the reservation is unchanged after a refused edit"
    );
}

#[test]
fn a_level_may_be_lowered_which_is_the_only_remedy_for_an_exhausted_budget() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL", "level": "tree" }),
    )
    .unwrap();
    let edited = h
        .call(
            &sid,
            "collectors.edit",
            json!({ "name": "c", "level": "timing" }),
        )
        .expect("lowering is permitted");
    assert_eq!(edited["level"], "timing");
}

#[test]
fn an_edit_re_runs_admission_so_it_cannot_arm_what_add_would_refuse() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    assert!(
        h.call(
            &sid,
            "collectors.edit",
            json!({ "name": "c", "filter": "d>=100, d<=50" }),
        )
        .is_err(),
        "an empty duration interval can never match, and edit must refuse it too"
    );
    // And a legal-but-surprising filter is armed WITH the warning.
    let edited = h
        .call(
            &sid,
            "collectors.edit",
            json!({ "name": "c", "filter": "SV=store_server" }),
        )
        .expect("armed");
    assert!(!edited["warnings"].as_array().unwrap().is_empty());
}

#[test]
fn an_empty_window_says_why_it_is_empty() {
    // `zeroed_by` is what lets a caller reading `matched: 0` tell "no traffic
    // yet" from "something emptied this window" — and WHICH something, because
    // "snapshot" means the run was kept while "reset" means it was discarded.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    let zeroed_by = |h: &Harness| {
        h.call(&sid, "collectors.list", json!({})).unwrap()["collectors"][0]["zeroed_by"].clone()
    };

    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    assert!(
        zeroed_by(&h).is_null(),
        "nothing has zeroed a fresh collector"
    );

    h.feed("default", &span("svc", "op", 10.0));
    h.call(&sid, "collectors.snapshot", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(zeroed_by(&h), "snapshot", "the run was kept");

    h.feed("default", &span("svc", "op", 10.0));
    h.call(&sid, "collectors.reset", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(zeroed_by(&h), "reset", "the run was discarded");

    h.feed("default", &span("svc", "op", 10.0));
    h.call(
        &sid,
        "collectors.edit",
        json!({ "name": "c", "filter": "sv=svc" }),
    )
    .unwrap();
    assert_eq!(zeroed_by(&h), "edit", "the definition changed under it");
}

#[test]
fn an_edit_that_cannot_be_made_durable_is_refused_and_changes_nothing() {
    // §7.1's ordering rule, exercised end to end: the write is the commit
    // point. If the mutation happened first and the write failed after it, the
    // daemon would hold a zeroed collector while the on-disk file still said
    // the old filter — and a restart would resurrect a definition the caller
    // believes they replaced.
    let d = tempfile::TempDir::new().unwrap();
    // Occupy the collectors/ path with a FILE, so every persist fails.
    std::fs::write(d.path().join("collectors"), "in the way").unwrap();
    let h = harness_in(Some(d.path().to_path_buf()));
    let sid = h.sessions.create_named("perf").unwrap();

    // Arming still works: a failed persist costs durability (logged), not the
    // measurement the caller asked for.
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "sv=svc" }),
    )
    .expect("armed despite the broken directory");
    h.feed("default", &span("svc", "op", 10.0));

    let err = h
        .call(
            &sid,
            "collectors.edit",
            json!({ "name": "c", "filter": "sn=op" }),
        )
        .unwrap_err();
    assert!(err.contains("durable"), "got: {err}");

    let got = h
        .call(&sid, "collectors.get", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(got["filter"], "sv=svc", "the definition did not change");
    assert_eq!(got["matched"], 1, "and the window was not zeroed");
}

#[test]
fn an_edit_that_changes_nothing_is_an_error_rather_than_a_silent_no_op() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    let err = h
        .call(&sid, "collectors.edit", json!({ "name": "c" }))
        .unwrap_err();
    assert!(err.contains("nothing to edit"), "got: {err}");
}

#[test]
fn re_pinning_a_collector_with_a_live_window_is_refused() {
    // Otherwise the recorded window and the domain it was measured on disagree.
    let h = harness();
    h.domains.insert(make_domain("t3"));
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    h.feed("default", &span("svc", "op", 10.0));
    let err = h
        .call(
            &sid,
            "collectors.edit",
            json!({ "name": "c", "domain": "t3" }),
        )
        .unwrap_err();
    assert!(err.contains("zeroed"), "got: {err}");

    // Snapshot it, and the re-pin goes through.
    h.call(&sid, "collectors.snapshot", json!({ "name": "c" }))
        .unwrap();
    let edited = h
        .call(
            &sid,
            "collectors.edit",
            json!({ "name": "c", "domain": "t3" }),
        )
        .expect("now permitted");
    assert_eq!(edited["domain"], "t3");
}

#[test]
fn an_edit_cannot_re_pin_to_a_domain_that_does_not_exist() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    let err = h
        .call(
            &sid,
            "collectors.edit",
            json!({ "name": "c", "domain": "nowhere" }),
        )
        .unwrap_err();
    assert!(err.contains("does not exist"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Deep-gate findings
// ---------------------------------------------------------------------------

#[test]
fn a_huge_group_key_list_is_refused_rather_than_allocated() {
    // The group-key column is reserved eagerly at CHUNK_RECORDS * width * 4
    // bytes, and the width is the one dimension max_sample_bytes does not
    // cover — so a tiny declared budget with a huge key list passed every gate
    // and then asked for gigabytes, which Vec::with_capacity answers by
    // ABORTING the process. One RPC call could stop the daemon.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    let many: Vec<String> = (0..50_000).map(|i| format!("k{i}")).collect();

    for method in ["collectors.add", "traces.profile"] {
        let err = h
            .call(
                &sid,
                method,
                json!({
                    "name": "c", "filter": "ALL",
                    "max_sample_bytes": 1024,
                    "group_keys": many,
                }),
            )
            .unwrap_err();
        assert!(
            err.contains("too many group_keys"),
            "{method} must refuse it: {err}"
        );
    }

    // And an edit cannot smuggle it in past the same gate.
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    assert!(h
        .call(
            &sid,
            "collectors.edit",
            json!({ "name": "c", "group_keys": many }),
        )
        .unwrap_err()
        .contains("too many group_keys"));
}

#[test]
fn an_absurd_warmup_does_not_silently_become_a_no_op() {
    // skip_warmup_ms arrives unclamped. A plain `+` overflows i64, and the
    // release profile has no overflow checks, so it wrapped to a large NEGATIVE
    // cutoff — excluding nothing and turning the cut into a silent no-op.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL", "level": "timing" }),
    )
    .unwrap();
    for i in 0..3 {
        h.feed("default", &span_at("svc", "op", 10.0, i * 10_000_000));
    }

    let got = h
        .call(
            &sid,
            "collectors.get",
            json!({ "name": "c", "skip_warmup_ms": 1e16 }),
        )
        .expect("must not panic");
    assert_eq!(
        got["sampled"]["sample_count"], 0,
        "an absurd cut excludes everything; wrapping would have excluded nothing"
    );
}

#[test]
fn a_rename_carries_its_collectors_and_does_not_inherit_a_displaced_session_s() {
    // Two defects at once. Leaving Entry::owner behind orphaned the renaming
    // session's own collectors: invisible to its owner-scoped list, unreachable
    // by session.drop, never swept, still holding the reservation. And a
    // DISPLACED holder's collectors — window and full history — became readable
    // and removable by whoever took the name.
    let h = harness();

    // A disconnected session already holds the target name, with a collector.
    let beta = h.sessions.create_named("beta").unwrap();
    h.call(
        &beta,
        "collectors.add",
        json!({ "name": "secret", "filter": "ALL" }),
    )
    .unwrap();
    h.sessions.disconnect(&beta);

    let alpha = h.sessions.create_named("alpha").unwrap();
    h.call(
        &alpha,
        "collectors.add",
        json!({ "name": "mine", "filter": "ALL" }),
    )
    .unwrap();

    h.call(&alpha, "session.rename", json!({ "name": "beta" }))
        .expect("renamed, displacing the stale holder");

    let now = SessionId::Named("beta".into());
    let listed = h.call(&now, "collectors.list", json!({})).unwrap();
    let names: Vec<&str> = listed["collectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"mine"), "kept its own: {names:?}");
    assert!(
        !names.contains(&"secret"),
        "must NOT inherit another conversation's measurements: {names:?}"
    );
    // And the displaced one released its budget rather than leaking it.
    assert_eq!(
        h.collectors.reserved_bytes(),
        DEFAULT_MAX_SAMPLE_BYTES,
        "exactly one collector's worth reserved"
    );
}

#[test]
fn a_re_pin_carries_the_metrics_handle_so_ingest_stays_attributable() {
    // The identity check behind ingest attribution is pointer equality on the
    // metrics handle. Moving the domain but not the handle made every later
    // read say "the pinned domain is gone or was recreated" about a domain that
    // exists and had just been re-pinned to — on the design's own repair path.
    let h = harness();
    h.domains.insert(make_domain("t3"));
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    h.call(
        &sid,
        "collectors.edit",
        json!({ "name": "c", "domain": "t3" }),
    )
    .expect("re-pinned");

    let got = h
        .call(&sid, "collectors.get", json!({ "name": "c" }))
        .unwrap();
    assert!(
        got["ingest"].is_object(),
        "ingest must still be attributable after a re-pin, got: {}",
        got["ingest"]
    );
    // And it collects on the new pin.
    h.feed("t3", &span("svc", "op", 5.0));
    assert_eq!(
        h.call(&sid, "collectors.get", json!({ "name": "c" }))
            .unwrap()["matched"],
        1
    );
}

#[test]
fn dropping_a_session_reclaims_collectors_even_when_the_session_is_already_gone() {
    // The state a restored-after-crash collector is in: its file came back but
    // its session did not. Releasing collectors AFTER the session drop meant
    // the `?` aborted on NotFound, so this call — the only way to reclaim that
    // budget — could not.
    let h = harness();
    let sid = SessionId::Named("ghost".into());
    h.collectors
        .add(
            &sid,
            &DomainId::default_domain(),
            Arc::new(ReceiverMetrics::new()),
            logmon_broker_core::collector::state::CollectorDef {
                name: "orphan".into(),
                filter_string: "ALL".into(),
                filter: logmon_broker_core::filter::parser::parse_filter("ALL").unwrap(),
                level: logmon_broker_core::collector::sample::Level::Tree,
                group_keys: vec![],
                max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
                description: None,
            },
            chrono::Utc::now(),
        )
        .expect("armed");
    assert!(h.collectors.reserved_bytes() > 0);

    let caller = h.sessions.create_named("live").unwrap();
    h.call(&caller, "session.drop", json!({ "name": "ghost" }))
        .expect("must succeed: there were collectors to reclaim");
    assert_eq!(
        h.collectors.reserved_bytes(),
        0,
        "the budget came back even though no session by that name existed"
    );
}

// ---------------------------------------------------------------------------
// Snapshot history (§6) — the driving workflow
// ---------------------------------------------------------------------------

#[test]
fn the_ab_workflow_records_two_runs_and_keeps_both() {
    // arm, run A, snapshot, run B, snapshot, then compare — the sequence the
    // whole feature exists for.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "cache", "filter": "sv=svc", "level": "tree" }),
    )
    .expect("armed");

    for i in 0..4 {
        h.feed("default", &span_at("svc", "op", 50.0, i * 100_000_000));
    }
    let a = h
        .call(
            &sid,
            "collectors.snapshot",
            json!({
                "name": "cache", "label": "no-cache",
                "description": "baseline, cache disabled",
                "meta": { "commit": "abc123" },
            }),
        )
        .expect("snapshot A");
    assert_eq!(a["label"], "no-cache");
    assert_eq!(a["exact"]["total_ms"], 200.0);
    assert_eq!(a["meta"]["commit"], "abc123");

    // The snapshot reset the window, so run B starts clean.
    for i in 0..4 {
        h.feed("default", &span_at("svc", "op", 10.0, i * 100_000_000));
    }
    let b = h
        .call(
            &sid,
            "collectors.snapshot",
            json!({ "name": "cache", "label": "with-cache" }),
        )
        .expect("snapshot B");
    assert_eq!(b["exact"]["total_ms"], 40.0, "run B is not run A plus B");

    // Both runs are still there, oldest first, each with its own context.
    let hist = h
        .call(&sid, "collectors.history", json!({ "name": "cache" }))
        .expect("history");
    assert_eq!(hist["count"], 2);
    assert_eq!(hist["evicted"], 0);
    assert_eq!(hist["snapshots"][0]["label"], "no-cache");
    assert_eq!(
        hist["snapshots"][0]["description"], "baseline, cache disabled",
        "the reason survives with the number"
    );
    assert_eq!(hist["snapshots"][1]["label"], "with-cache");

    // And an individual run is readable by label.
    let one = h
        .call(
            &sid,
            "collectors.get",
            json!({ "name": "cache", "snapshot": "no-cache" }),
        )
        .expect("by label");
    assert_eq!(one["exact"]["total_ms"], 200.0);

    // The live collector is empty — everything went into the two records.
    let live = h
        .call(&sid, "collectors.get", json!({ "name": "cache" }))
        .expect("live");
    assert_eq!(live["matched"], 0);
}

#[test]
fn a_snapshot_can_record_without_resetting() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    h.feed("default", &span("svc", "op", 10.0));

    h.call(
        &sid,
        "collectors.snapshot",
        json!({ "name": "c", "label": "mark", "reset": false }),
    )
    .expect("recorded");
    let live = h
        .call(&sid, "collectors.get", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(live["matched"], 1, "the window kept running");
}

#[test]
fn a_snapshot_records_the_definition_it_was_taken_under() {
    // §6.3, and the case it exists for: the collector changes between runs. A
    // reader of the old snapshot must see the old filter, not today's.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "sv=svc", "level": "timing" }),
    )
    .unwrap();
    h.feed("default", &span("svc", "op", 10.0));
    h.call(
        &sid,
        "collectors.snapshot",
        json!({ "name": "c", "label": "before" }),
    )
    .unwrap();

    let recorded = h
        .call(
            &sid,
            "collectors.get",
            json!({ "name": "c", "snapshot": "before" }),
        )
        .unwrap();
    assert_eq!(recorded["filter"], "sv=svc");
    assert_eq!(recorded["level"], "timing");
}

#[test]
fn merging_repeats_adds_them_and_refuses_to_merge_what_cannot_merge() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL", "level": "tree" }),
    )
    .unwrap();

    // Three repeats of the same configuration: 100, 110, 120 ms.
    for (i, total) in [100.0, 110.0, 120.0].iter().enumerate() {
        for k in 0..10 {
            h.feed(
                "default",
                &span_at("svc", "op", total / 10.0, (i * 10 + k) as i64 * 10_000_000),
            );
        }
        h.call(
            &sid,
            "collectors.snapshot",
            json!({ "name": "c", "label": format!("r{i}") }),
        )
        .unwrap();
    }

    let hist = h
        .call(
            &sid,
            "collectors.history",
            json!({ "name": "c", "merge": true }),
        )
        .expect("merged");
    assert_eq!(hist["merged"]["count"], 30);
    assert_eq!(hist["merged"]["total_ms"], 330.0);
    assert!(hist["merged_estimated"]["p50_ms"].is_number());

    // The floor, with its caveat attached.
    assert_eq!(hist["floor"]["runs"], 3);
    assert_eq!(hist["floor"]["min"], 100.0);
    assert_eq!(hist["floor"]["max"], 120.0);
    let cv = hist["floor"]["cv_pct"].as_f64().expect("three runs");
    assert!((cv - 9.0909).abs() < 0.01, "got {cv}");
    assert!(hist["floor"]["caveat"]
        .as_str()
        .unwrap()
        .contains("ingest loss"));

    // Self time across separate runs is not a self time, and the result says
    // so rather than quietly omitting it.
    let sup = hist["suppressed"].as_array().unwrap();
    assert!(
        sup.iter().any(|s| s["field"] == "merged.sampled"),
        "got {sup:?}"
    );
}

#[test]
fn a_single_run_reports_the_floor_as_unknown() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    h.feed("default", &span("svc", "op", 10.0));
    h.call(&sid, "collectors.snapshot", json!({ "name": "c" }))
        .unwrap();

    let hist = h
        .call(
            &sid,
            "collectors.history",
            json!({ "name": "c", "merge": true }),
        )
        .unwrap();
    assert_eq!(hist["floor"]["runs"], 1);
    assert!(
        hist["floor"]["cv_pct"].is_null(),
        "unknown, not zero — zero would license calling any difference real"
    );
}

#[test]
fn an_auto_labelled_snapshot_gets_a_sequential_name() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    for expect in ["snapshot-1", "snapshot-2", "snapshot-3"] {
        let s = h
            .call(&sid, "collectors.snapshot", json!({ "name": "c" }))
            .unwrap();
        assert_eq!(s["label"], expect);
    }
}

#[test]
fn a_duplicate_snapshot_label_is_refused() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    h.call(
        &sid,
        "collectors.snapshot",
        json!({ "name": "c", "label": "base" }),
    )
    .unwrap();
    let err = h
        .call(
            &sid,
            "collectors.snapshot",
            json!({ "name": "c", "label": "base" }),
        )
        .unwrap_err();
    assert!(err.contains("already exists"), "got: {err}");
}

#[test]
fn history_survives_a_reset_and_the_reset_does_not_record() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    h.feed("default", &span("svc", "op", 10.0));
    h.call(
        &sid,
        "collectors.snapshot",
        json!({ "name": "c", "label": "kept" }),
    )
    .unwrap();

    h.feed("default", &span("svc", "op", 99.0));
    h.call(&sid, "collectors.reset", json!({ "name": "c" }))
        .expect("discarded");

    let hist = h
        .call(&sid, "collectors.history", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(hist["count"], 1, "the reset recorded nothing...");
    assert_eq!(hist["snapshots"][0]["label"], "kept", "...and took nothing");
}

#[test]
fn limit_returns_the_most_recent_runs_oldest_first() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    for _ in 0..5 {
        h.call(&sid, "collectors.snapshot", json!({ "name": "c" }))
            .unwrap();
    }
    let hist = h
        .call(
            &sid,
            "collectors.history",
            json!({ "name": "c", "limit": 2 }),
        )
        .unwrap();
    assert_eq!(hist["count"], 2);
    assert_eq!(hist["snapshots"][0]["label"], "snapshot-4");
    assert_eq!(hist["snapshots"][1]["label"], "snapshot-5");
}

#[test]
fn a_snapshot_policy_above_the_level_is_refused_not_downgraded() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL", "level": "scalar" }),
    )
    .unwrap();
    let err = h
        .call(
            &sid,
            "collectors.snapshot",
            json!({ "name": "c", "projections": true }),
        )
        .unwrap_err();
    assert!(
        err.contains("scalar") && err.contains("projections"),
        "got: {err}"
    );
    // Not asking for them works at any level.
    h.call(
        &sid,
        "collectors.snapshot",
        json!({ "name": "c", "projections": false }),
    )
    .expect("recorded without projections");
}

#[test]
fn a_snapshot_stores_its_projections_because_the_samples_are_not_kept() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL", "level": "tree" }),
    )
    .unwrap();
    for i in 0..4 {
        h.feed("default", &span_at("svc", "op", 25.0, i * 100_000_000));
    }
    let s = h
        .call(&sid, "collectors.snapshot", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(s["sampled"]["sample_count"], 4);
    assert_eq!(s["sampled"]["p50_ms"], 25.0);
    assert_eq!(
        s["sampled"]["complete"], true,
        "and whether it was truncated travels with it"
    );
}

// ---------------------------------------------------------------------------
// Ingest accounting (§5.1.1)
// ---------------------------------------------------------------------------

#[test]
fn span_loss_is_attributed_to_the_window_it_happened_in() {
    use logmon_broker_core::receiver::TraceTransport;
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    let d = h.domains.get(&DomainId::default_domain()).unwrap();

    // Loss before arming belongs to nobody: the collector was not running.
    d.metrics.record_trace_batch_shed(TraceTransport::OtlpHttp);
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .expect("armed");
    let got = h
        .call(&sid, "collectors.get", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(
        got["ingest"]["shed_batches"], 0,
        "a batch shed before arming is not this window's loss"
    );

    d.metrics.record_trace_batch_shed(TraceTransport::OtlpGrpc);
    d.metrics.record_trace_malformed(TraceTransport::OtlpHttp);
    let got = h
        .call(&sid, "collectors.get", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(got["ingest"]["shed_batches"], 1);
    assert_eq!(got["ingest"]["malformed_dropped"], 1);
    assert_eq!(
        got["ingest"]["attribution"], "domain",
        "the counters are unfiltered, and the result says so"
    );

    // A reset starts a new window, and the baseline must move with it — or the
    // pre-reset loss is charged to the run after it.
    h.call(&sid, "collectors.reset", json!({ "name": "c" }))
        .expect("reset");
    let got = h
        .call(&sid, "collectors.get", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(
        got["ingest"]["shed_batches"], 0,
        "the previous window's loss does not follow the reset"
    );
    assert_eq!(got["ingest"]["malformed_dropped"], 0);

    d.metrics.record_trace_batch_shed(TraceTransport::OtlpHttp);
    let got = h
        .call(&sid, "collectors.get", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(
        got["ingest"]["shed_batches"], 1,
        "and the new window counts"
    );
}

#[test]
fn a_domain_recreated_under_a_collector_withholds_the_ingest_block() {
    // Pointer identity, not the name: a fresh domain with the same name has
    // fresh counters at zero, and a delta against the old baseline would be
    // arithmetic across two unrelated sequences.
    let h = harness();
    h.domains.insert(make_domain("t3"));
    let sid = h.sessions.create_named("A").unwrap();
    h.call(&sid, "domains.use", json!({ "name": "t3" }))
        .unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .expect("armed");
    assert!(
        h.call(&sid, "collectors.get", json!({ "name": "c" }))
            .unwrap()["ingest"]
            .is_object(),
        "same instance, so a delta is meaningful"
    );

    h.domains.insert(make_domain("t3")); // same name, new counters
    let got = h
        .call(&sid, "collectors.get", json!({ "name": "c" }))
        .unwrap();
    assert!(got["ingest"].is_null());
    let sup = got["suppressed"].as_array().unwrap();
    assert!(
        sup.iter().any(|s| s["field"] == "ingest"),
        "and it says why: {sup:?}"
    );
}

// ---------------------------------------------------------------------------
// Admission (§7, V24)
// ---------------------------------------------------------------------------

#[test]
fn v24_a_filter_that_provably_matches_nothing_is_refused() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    for filter in ["d>=100, d<=50", "d>=NaN"] {
        let err = h
            .call(
                &sid,
                "collectors.add",
                json!({ "name": "c", "filter": filter }),
            )
            .unwrap_err();
        assert!(
            !err.is_empty(),
            "`{filter}` can never match and must be refused, not armed"
        );
    }
}

#[test]
fn v24_a_surprising_but_legal_filter_is_armed_with_a_warning() {
    // The flagship failure: a filter with the shift key stuck. It parses, it
    // arms, and it matches nothing — so the warning is the whole defence.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    let added = h
        .call(
            &sid,
            "collectors.add",
            json!({ "name": "c", "filter": "SV=store_server" }),
        )
        .expect("armed, never refused");
    let warnings = added["warnings"].as_array().expect("a warnings list");
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap().contains("SV")),
        "the offending qualifier is named: {warnings:?}"
    );
}

#[test]
fn a_collector_name_that_could_reach_the_filesystem_is_refused() {
    // Collector names end up in filenames beside state.json, and in the
    // `collector@label` syntax.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    for name in ["../escape", "a/b", "with@label", ""] {
        assert!(
            h.call(
                &sid,
                "collectors.add",
                json!({ "name": name, "filter": "ALL" })
            )
            .is_err(),
            "`{name}` must be refused"
        );
    }
}

#[test]
fn an_unknown_level_names_the_ones_that_exist() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    let err = h
        .call(
            &sid,
            "collectors.add",
            json!({ "name": "c", "filter": "ALL", "level": "detailed" }),
        )
        .unwrap_err();
    assert!(err.contains("scalar") && err.contains("timing") && err.contains("tree"));
}

// ---------------------------------------------------------------------------
// Breakdowns through the wire
// ---------------------------------------------------------------------------

#[test]
fn group_by_group_splits_an_ab_by_a_boolean_attribute() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({
            "name": "ab",
            "filter": "sv=svc",
            "level": "timing",
            "group_keys": ["cache.enabled"],
        }),
    )
    .expect("armed");

    for i in 0..3 {
        let mut on = span_at("svc", "op", 10.0, i * 100_000_000);
        on.attributes.insert("cache.enabled".into(), json!(true));
        let mut off = span_at("svc", "op", 40.0, i * 100_000_000 + 50_000_000);
        off.attributes.insert("cache.enabled".into(), json!(false));
        h.feed("default", &on);
        h.feed("default", &off);
    }

    let got = h
        .call(
            &sid,
            "collectors.get",
            json!({ "name": "ab", "group_by": "group" }),
        )
        .expect("read");
    assert_eq!(got["grouped_by"], "group");
    let groups = got["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["key"], "false");
    assert_eq!(groups[0]["exact"]["total_ms"], 120.0);
    assert_eq!(groups[1]["key"], "true");
    assert_eq!(groups[1]["exact"]["total_ms"], 30.0);
}

#[test]
fn an_unknown_group_by_names_the_ones_that_exist() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL" }),
    )
    .unwrap();
    let err = h
        .call(
            &sid,
            "collectors.get",
            json!({ "name": "c", "group_by": "service" }),
        )
        .unwrap_err();
    assert!(err.contains("name") && err.contains("path"), "got: {err}");
}

#[test]
fn a_scalar_collector_says_why_it_has_no_sampled_block() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "ALL", "level": "scalar" }),
    )
    .unwrap();
    h.feed("default", &span("svc", "op", 10.0));

    let got = h
        .call(&sid, "collectors.get", json!({ "name": "c" }))
        .unwrap();
    assert_eq!(got["exact"]["count"], 1, "the exact tier still works");
    assert!(got["sampled"].is_null());
    let sup = got["suppressed"].as_array().unwrap();
    assert!(
        sup.iter()
            .any(|s| s["field"] == "sampled" && s["reason"].as_str().unwrap().contains("scalar")),
        "a null must come with a reason: {sup:?}"
    );
}

// ---------------------------------------------------------------------------
// traces.profile
// ---------------------------------------------------------------------------

#[test]
fn traces_profile_reads_what_is_already_in_the_buffer() {
    // The complement of a collector: a collector must be armed before the run
    // it measures, this looks back at one that already happened.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    for i in 0..4 {
        h.feed("default", &span_at("svc", "op", 25.0, i * 100_000_000));
    }
    h.feed("default", &span("other", "elsewhere", 900.0));

    let got = h
        .call(&sid, "traces.profile", json!({ "filter": "sv=svc" }))
        .expect("profiled");
    assert!(got["collector"].is_null(), "nothing was armed");
    assert_eq!(got["matched"], 4);
    assert_eq!(got["exact"]["total_ms"], 100.0);
    assert!(got["description"]
        .as_str()
        .unwrap()
        .contains("ad-hoc profile"));
}

#[test]
fn an_ad_hoc_profile_does_not_claim_the_domain_is_gone() {
    // Caught by the post-deploy smoke test: `traces.profile` reused the
    // collector's "pinned domain is gone or was recreated" reason, which is a
    // false statement about the reader's system. A suppression reason is only
    // worth having if it can be trusted.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.feed("default", &span("svc", "op", 5.0));

    let got = h.call(&sid, "traces.profile", json!({})).expect("profiled");
    assert!(got["ingest"].is_null());
    let sup = got["suppressed"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["field"] == "ingest")
        .expect("a reason");
    let reason = sup["reason"].as_str().unwrap();
    assert!(
        !reason.contains("gone") && !reason.contains("recreated"),
        "nothing is wrong with the domain: {reason}"
    );
    assert!(reason.contains("no window"), "got: {reason}");
    assert!(sup["remedy"].is_string(), "and what to do instead");
}

#[test]
fn traces_profile_refuses_a_cursor_because_a_profile_must_repeat() {
    // A cursor is read-and-advance, so a second identical call would return
    // less than the first.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    let err = h
        .call(&sid, "traces.profile", json!({ "filter": "c>=mycursor" }))
        .unwrap_err();
    assert!(err.contains("cursor"), "got: {err}");
}

#[test]
fn traces_profile_returns_the_same_numbers_twice() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    for i in 0..3 {
        h.feed("default", &span_at("svc", "op", 7.0, i * 10_000_000));
    }
    let a = h.call(&sid, "traces.profile", json!({})).unwrap();
    let b = h.call(&sid, "traces.profile", json!({})).unwrap();
    assert_eq!(
        a["exact"], b["exact"],
        "a profile does not consume anything"
    );
    assert_eq!(a["matched"], b["matched"]);
}

// ---------------------------------------------------------------------------
// traces.slow — the §1.1 repair
// ---------------------------------------------------------------------------

#[test]
fn v12_the_grouped_arm_aggregates_the_full_population_not_the_slow_tail() {
    // 100 fast spans and 3 slow ones, all named "query". Grouping the output
    // of `slow_spans` gave avg 500 ms — the average of the three above the
    // floor — reported as the average for "query". The honest answer is that
    // "query" averages about 15 ms and has a tail at 500.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    let d = h.domains.get(&DomainId::default_domain()).unwrap();
    for _ in 0..100 {
        d.span_store.insert(span("svc", "query", 10.0));
    }
    for _ in 0..3 {
        d.span_store.insert(span("svc", "query", 500.0));
    }

    let got = h
        .call(
            &sid,
            "traces.slow",
            json!({ "min_duration_ms": 100.0, "group_by": "name" }),
        )
        .expect("grouped");

    assert_eq!(got["population"], 103, "every matching span was aggregated");
    assert_eq!(got["display_floor_ms"], 100.0);
    let g = &got["groups"][0];
    assert_eq!(g["name"], "query");
    assert_eq!(g["count"], 103, "not 3, and not 20");
    let avg = g["avg_ms"].as_f64().unwrap();
    assert!(
        (24.0..25.0).contains(&avg),
        "avg over all 103 spans is 24.3, not 500: got {avg}"
    );
    assert_eq!(g["max_ms"], 500.0, "the outlier is visible as an outlier");
    assert_eq!(
        g["p50_ms"], 10.0,
        "the median of the population, not of the tail"
    );
}

#[test]
fn v12_the_floor_selects_names_and_the_rank_follows_the_lower_convention() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    let d = h.domains.get(&DomainId::default_domain()).unwrap();
    // "fast" never reaches the floor and must not appear at all.
    for _ in 0..20 {
        d.span_store.insert(span("svc", "fast", 1.0));
    }
    // "slow" has exactly 20 spans at 1..20 ms — p95 by §5.7 is
    // floor(1 + 0.95*19) = 19 → the 19th smallest = 19 ms, NOT the maximum.
    for i in 1..=20 {
        d.span_store.insert(span("svc", "slow", i as f64));
    }

    let got = h
        .call(
            &sid,
            "traces.slow",
            json!({ "min_duration_ms": 15.0, "group_by": "name" }),
        )
        .unwrap();
    let groups = got["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "only names with a span above the floor");
    assert_eq!(groups[0]["name"], "slow");
    assert_eq!(
        groups[0]["p95_ms"], 19.0,
        "floor(n*0.95) returned the maximum at n=20; the lower quantile is 19"
    );
    assert_eq!(groups[0]["max_ms"], 20.0);
}

#[test]
fn the_ungrouped_arm_still_returns_the_slowest_n_above_the_floor() {
    // Unchanged on purpose: a list of the slowest spans is not an aggregate,
    // and truncating it is exactly what it is for.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    let d = h.domains.get(&DomainId::default_domain()).unwrap();
    for i in 1..=50 {
        d.span_store.insert(span("svc", "op", i as f64 * 10.0));
    }
    let got = h
        .call(
            &sid,
            "traces.slow",
            json!({ "min_duration_ms": 100.0, "count": 5 }),
        )
        .unwrap();
    assert_eq!(got["count"], 5);
    assert_eq!(got["spans"][0]["duration_ms"], 500.0);
}

// ---------------------------------------------------------------------------
// collectors.diff (spec §5.6)
// ---------------------------------------------------------------------------

/// Arm a collector, feed `n` spans of `each_ms`, snapshot under `label`.
fn run_and_snapshot(h: &Harness, sid: &SessionId, name: &str, label: &str, n: usize, each_ms: f64) {
    for i in 0..n {
        h.feed(
            "default",
            &span_at("store_server", "reconcile", each_ms, i as i64 * 50_000_000),
        );
    }
    h.call(
        sid,
        "collectors.snapshot",
        json!({ "name": name, "label": label }),
    )
    .expect("snapshot taken");
}

fn armed(h: &Harness, sid: &SessionId, name: &str) {
    h.call(
        sid,
        "collectors.add",
        json!({ "name": name, "filter": "sv=store_server", "level": "tree" }),
    )
    .expect("armed");
}

fn diff_row<'a>(got: &'a Value, metric: &str, tier: &str) -> &'a Value {
    got["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|r| r["metric"] == metric && r["tier"] == tier)
        .unwrap_or_else(|| panic!("no {tier} row for {metric} in {got:#}"))
}

#[test]
fn two_recorded_runs_diff_by_label() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    run_and_snapshot(&h, &sid, "cache", "before", 20, 10.0);
    run_and_snapshot(&h, &sid, "cache", "after", 20, 5.0);

    let got = h
        .call(
            &sid,
            "collectors.diff",
            json!({ "a": "cache@before", "b": "cache@after" }),
        )
        .expect("diffed");

    assert_eq!(got["a"]["spec"], "cache@before");
    assert_eq!(got["b"]["spec"], "cache@after");
    assert_eq!(got["level"], "tree");

    let total = diff_row(&got, "total_ms", "exact");
    assert_eq!(total["a"], 200.0);
    assert_eq!(total["b"], 100.0);
    assert_eq!(total["delta"], -100.0);
    assert_eq!(total["delta_pct"], -50.0);

    // Both arms are single runs, so the floor is unknown — stated, never zero.
    assert_eq!(total["threshold_basis"], "unknown");
    assert!(total.get("threshold_abs").is_none());
    assert_eq!(
        got["trustworthy"], false,
        "a two-single-run comparison cannot separate a change from noise"
    );

    // And the definition each arm reports is the one recorded with it.
    assert_eq!(got["a"]["filter"], "sv=store_server");
    assert_eq!(got["a"]["aggregation"], "single run: before");
}

#[test]
fn an_estimated_row_carries_the_error_bound_over_the_wire() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    run_and_snapshot(&h, &sid, "cache", "before", 40, 100.0);
    run_and_snapshot(&h, &sid, "cache", "after", 40, 50.0);

    let got = h
        .call(
            &sid,
            "collectors.diff",
            json!({ "a": "cache@before", "b": "cache@after" }),
        )
        .unwrap();
    let p95 = diff_row(&got, "p95_ms", "estimated");
    let bound = p95["error_bound_pct"].as_f64().expect("bound present");
    // A 50 % change is far outside the resolution floor, so the bound is small
    // and the delta stands.
    assert!(bound < 10.0, "a halving is comfortably resolvable: {bound}");
    assert!(p95.get("suppressed").is_none() || p95["suppressed"] == false);
    assert_eq!(p95["threshold_basis"], "measurement-resolution");
    assert_eq!(p95["threshold_pct"], 2.0, "2α at α=1%");
}

#[test]
fn an_unresolvable_estimated_delta_is_suppressed_over_the_wire() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    run_and_snapshot(&h, &sid, "cache", "before", 40, 100.0);
    run_and_snapshot(&h, &sid, "cache", "after", 40, 100.0);

    let got = h
        .call(
            &sid,
            "collectors.diff",
            json!({ "a": "cache@before", "b": "cache@after" }),
        )
        .unwrap();
    let p95 = diff_row(&got, "p95_ms", "estimated");
    assert_eq!(p95["suppressed"], true, "no change is not a small change");
    // The printed threshold is the applied one, in the metric's own units.
    let t = p95["threshold_abs"].as_f64().expect("printed");
    assert!(p95["delta"].as_f64().unwrap().abs() <= t);
}

#[test]
fn the_wildcard_arm_merges_every_recorded_run_and_produces_a_real_floor() {
    // The only arm shape that yields a run-to-run floor, which is what makes a
    // delta distinguishable from scheduling noise (§6.5).
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "base");
    for (i, ms) in [10.0, 11.0, 12.0].iter().enumerate() {
        run_and_snapshot(&h, &sid, "base", &format!("r{i}"), 20, *ms);
    }
    armed(&h, &sid, "cand");
    for (i, ms) in [5.0, 5.5, 6.0].iter().enumerate() {
        run_and_snapshot(&h, &sid, "cand", &format!("s{i}"), 20, *ms);
    }

    let got = h
        .call(
            &sid,
            "collectors.diff",
            json!({ "a": "base@*", "b": "cand@*", "allow_mismatch": false }),
        )
        .expect("diffed");

    assert!(got["a"]["aggregation"]
        .as_str()
        .unwrap()
        .contains("merged across 3 runs"));
    let floor = &got["a"]["floor"];
    assert_eq!(floor["runs"], 3);
    assert!(floor["cv_pct"].as_f64().is_some(), "three runs have a CV");
    assert!(
        floor["caveat"].as_str().unwrap().contains("scheduling"),
        "and the caveat travels with it"
    );

    let total = diff_row(&got, "total_ms", "exact");
    assert_eq!(total["a"], 660.0, "20 spans x (10+11+12) ms");
    assert_eq!(total["b"], 330.0, "20 spans x (5+5.5+6) ms");
    assert_eq!(
        total["threshold_basis"], "run-to-run",
        "with a real floor the exact rows finally have a threshold"
    );
    assert!(total["threshold_abs"].as_f64().unwrap() > 0.0);

    // Two different collectors is permitted and labelled, not refused.
    assert!(got["marks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["kind"] == "collector"));
    // Nothing substantive differs and both arms are multi-run.
    assert_eq!(got["trustworthy"], true);
}

#[test]
fn a_wildcard_arm_with_no_recorded_runs_says_what_to_do() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    let err = h
        .call(
            &sid,
            "collectors.diff",
            json!({ "a": "cache@*", "b": "cache" }),
        )
        .expect_err("refused");
    assert!(err.contains("no recorded runs"), "{err}");
    assert!(err.contains("collectors.snapshot"), "and the remedy: {err}");
}

#[test]
fn the_live_window_is_a_valid_arm() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    run_and_snapshot(&h, &sid, "cache", "before", 10, 20.0);
    // Post-snapshot spans land in the fresh live window.
    for i in 0..10 {
        h.feed(
            "default",
            &span_at("store_server", "reconcile", 8.0, i * 50_000_000),
        );
    }

    let got = h
        .call(
            &sid,
            "collectors.diff",
            json!({ "a": "cache@before", "b": "cache" }),
        )
        .expect("diffed");
    assert_eq!(
        got["b"]["aggregation"],
        "live window (single run, still open)"
    );
    let total = diff_row(&got, "total_ms", "exact");
    assert_eq!(total["a"], 200.0);
    assert_eq!(total["b"], 80.0);
}

#[test]
fn a_genuine_filter_difference_is_refused_until_allow_mismatch() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "reads", "filter": "sn=reconcile" }),
    )
    .unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "writes", "filter": "sn=commit" }),
    )
    .unwrap();
    for i in 0..5 {
        h.feed(
            "default",
            &span_at("store_server", "reconcile", 10.0, i * 50_000_000),
        );
        h.feed(
            "default",
            &span_at("store_server", "commit", 20.0, i * 50_000_000),
        );
    }

    let err = h
        .call(
            &sid,
            "collectors.diff",
            json!({ "a": "reads", "b": "writes" }),
        )
        .expect_err("blocked");
    assert!(err.contains("different span populations"), "{err}");
    assert!(err.contains("allow_mismatch"), "names the flag: {err}");

    let got = h
        .call(
            &sid,
            "collectors.diff",
            json!({ "a": "reads", "b": "writes", "allow_mismatch": true }),
        )
        .expect("permitted");
    let mark = got["marks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["kind"] == "filter")
        .expect("marked");
    assert_eq!(mark["overridden_by"], "allow_mismatch");
    assert_eq!(got["trustworthy"], false);
}

#[test]
fn a_diff_breakdown_by_name_pairs_rows_and_ranks_by_movement() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    for i in 0..5 {
        h.feed(
            "default",
            &span_at("store_server", "hot", 100.0, i * 50_000_000),
        );
        h.feed(
            "default",
            &span_at("store_server", "cold", 10.0, i * 50_000_000),
        );
    }
    h.call(
        &sid,
        "collectors.snapshot",
        json!({ "name": "cache", "label": "before" }),
    )
    .unwrap();
    for i in 0..5 {
        h.feed(
            "default",
            &span_at("store_server", "hot", 20.0, i * 50_000_000),
        );
        h.feed(
            "default",
            &span_at("store_server", "cold", 10.0, i * 50_000_000),
        );
    }
    h.call(
        &sid,
        "collectors.snapshot",
        json!({ "name": "cache", "label": "after" }),
    )
    .unwrap();

    let got = h
        .call(
            &sid,
            "collectors.diff",
            json!({ "a": "cache@before", "b": "cache@after", "group_by": "name" }),
        )
        .expect("diffed");
    assert_eq!(got["grouped_by"], "name");
    let groups = got["groups"].as_array().unwrap();
    assert_eq!(groups[0]["key"], "hot", "ranked by how much it moved");
    assert_eq!(groups[0]["presence"], "both");

    let hot_total = groups[0]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["metric"] == "total_ms")
        .unwrap();
    assert_eq!(hot_total["delta"], -400.0, "500ms became 100ms");
}

#[test]
fn trace_and_path_are_refused_as_diff_axes_with_the_reason() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    run_and_snapshot(&h, &sid, "cache", "a", 5, 10.0);
    run_and_snapshot(&h, &sid, "cache", "b", 5, 10.0);

    for axis in ["trace", "path"] {
        let err = h
            .call(
                &sid,
                "collectors.diff",
                json!({ "a": "cache@a", "b": "cache@b", "group_by": axis }),
            )
            .expect_err("refused");
        assert!(err.contains("does not retain"), "{axis}: {err}");
    }
}

#[test]
fn an_unknown_arm_names_the_collector_that_is_missing() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    let err = h
        .call(
            &sid,
            "collectors.diff",
            json!({ "a": "cache", "b": "nope" }),
        )
        .expect_err("refused");
    assert!(err.contains("nope"), "{err}");

    let err = h
        .call(
            &sid,
            "collectors.diff",
            json!({ "a": "cache", "b": "cache@absent" }),
        )
        .expect_err("refused");
    assert!(err.contains("absent"), "{err}");
}

#[test]
fn a_diff_arm_belongs_to_the_calling_session_only() {
    // Collectors are per-session; a diff must not become a way to read another
    // session's runs by naming them.
    let h = harness();
    let a = h.sessions.create_named("A").unwrap();
    let b = h.sessions.create_named("B").unwrap();
    armed(&h, &a, "cache");
    run_and_snapshot(&h, &a, "cache", "r1", 5, 10.0);

    let err = h
        .call(
            &b,
            "collectors.diff",
            json!({ "a": "cache@r1", "b": "cache" }),
        )
        .expect_err("not visible to B");
    assert!(err.contains("cache"), "{err}");
}

// ---------------------------------------------------------------------------
// collectors.document (spec §9)
// ---------------------------------------------------------------------------

#[test]
fn a_document_renders_end_to_end_with_both_arms_and_a_sidecar() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    run_and_snapshot(&h, &sid, "cache", "before", 20, 10.0);
    run_and_snapshot(&h, &sid, "cache", "after", 20, 5.0);

    let got = h
        .call(
            &sid,
            "collectors.document",
            json!({
                "names": ["cache@before", "cache@after"],
                "question": "did the cache help",
            }),
        )
        .expect("rendered");

    assert_eq!(got["format"], "md");
    let content = got["content"].as_str().expect("content");
    assert!(content.starts_with("---\n"), "front matter first");
    assert!(content.contains("question: \"did the cache help\""));
    assert!(content.contains("## 1. What moved"));
    assert!(content.contains("## 5. Definitions and how to read the numbers"));
    assert_eq!(got["bytes"], content.len() as u64);

    // §9.9: the daemon returns bytes; the client writes them. So there is no
    // `path` parameter here and nothing was written.
    let name = got["sidecar_name"].as_str().expect("sidecar named");
    assert!(content.contains(name), "the front matter names the sidecar");
    assert!(got["sidecar_content"]
        .as_str()
        .expect("sidecar body")
        .contains("percentiles_ms"));
}

#[test]
fn a_document_of_merged_arms_is_trustworthy_and_a_single_run_one_is_not() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "base");
    for (i, ms) in [10.0, 11.0, 12.0].iter().enumerate() {
        run_and_snapshot(&h, &sid, "base", &format!("r{i}"), 20, *ms);
    }
    armed(&h, &sid, "cand");
    for (i, ms) in [5.0, 5.5, 6.0].iter().enumerate() {
        run_and_snapshot(&h, &sid, "cand", &format!("s{i}"), 20, *ms);
    }

    let merged = h
        .call(
            &sid,
            "collectors.document",
            json!({ "names": ["base@*", "cand@*"] }),
        )
        .expect("rendered");
    let c = merged["content"].as_str().unwrap();
    assert!(c.contains("trustworthy: true"), "{c}");
    assert!(c.contains("every arm merges several runs"));
    assert!(!c.contains("**Single-run arm.**"));

    let single = h
        .call(
            &sid,
            "collectors.document",
            json!({ "names": ["base@r0", "cand@s0"] }),
        )
        .expect("rendered");
    let c = single["content"].as_str().unwrap();
    assert!(c.contains("trustworthy: false"));
    assert!(c.contains("**Single-run arm.**"), "with its remedy");
}

#[test]
fn a_single_arm_document_is_permitted() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    run_and_snapshot(&h, &sid, "cache", "only", 10, 10.0);

    let got = h
        .call(
            &sid,
            "collectors.document",
            json!({ "names": ["cache@only"] }),
        )
        .expect("rendered");
    let c = got["content"].as_str().unwrap();
    assert!(c.contains("describes **one** arm"));
    assert!(c.contains("## 3. Per-run detail"));
}

#[test]
fn the_json_format_is_machine_readable_and_carries_the_comparisons() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    run_and_snapshot(&h, &sid, "cache", "before", 20, 10.0);
    run_and_snapshot(&h, &sid, "cache", "after", 20, 5.0);

    let got = h
        .call(
            &sid,
            "collectors.document",
            json!({ "names": ["cache@before", "cache@after"], "format": "json" }),
        )
        .expect("rendered");
    assert_eq!(got["format"], "json");
    let v: Value = serde_json::from_str(got["content"].as_str().unwrap()).expect("valid json");
    assert_eq!(v["trustworthy"], false);
    assert_eq!(v["correctness_evidence"], "unknown");
    assert!(v["comparisons"][0]["rows"].as_array().unwrap().len() > 4);
}

#[test]
fn folded_output_matches_the_importer_regex_and_uses_integer_microseconds() {
    // §9.8: speedscope matches ^(.*) (\d+)$ and drops a non-matching line
    // SILENTLY, while flamegraph.pl accepts decimals — so a decimal would
    // render in one tool and vanish without warning in the other.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    run_and_snapshot(&h, &sid, "cache", "r", 10, 10.0);

    let got = h
        .call(
            &sid,
            "collectors.document",
            json!({ "names": ["cache@r"], "format": "folded" }),
        )
        .expect("rendered");
    assert_eq!(got["format"], "folded");
    let content = got["content"].as_str().unwrap();
    assert!(!content.is_empty(), "the snapshot recorded paths");
    for line in content.lines() {
        assert!(!line.contains('.'), "no decimals: {line:?}");
        let (frames, count) = line.rsplit_once(' ').expect("one ASCII space: {line:?}");
        assert!(!frames.is_empty());
        assert!(count.parse::<u64>().is_ok(), "{line:?}");
    }
    // The sidecar carries the invocation, because collapsed-stack format has no
    // unit concept of its own.
    assert!(got["sidecar_content"]
        .as_str()
        .unwrap()
        .contains("flamegraph.pl --countname us --nametype Span"));
}

#[test]
fn folded_is_refused_below_tree_with_the_level_and_the_remedy() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    h.call(
        &sid,
        "collectors.add",
        json!({ "name": "flat", "filter": "sv=store_server", "level": "timing" }),
    )
    .unwrap();
    run_and_snapshot(&h, &sid, "flat", "r", 5, 10.0);

    let err = h
        .call(
            &sid,
            "collectors.document",
            json!({ "names": ["flat@r"], "format": "folded" }),
        )
        .expect_err("refused");
    assert!(err.contains("level `timing`"), "{err}");
    assert!(err.contains("Re-run at level `tree`"), "{err}");
}

#[test]
fn a_document_with_no_arms_says_what_an_arm_looks_like() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    let err = h
        .call(&sid, "collectors.document", json!({ "names": [] }))
        .expect_err("refused");
    assert!(err.contains("names"), "{err}");
    assert!(err.contains("baseline"), "{err}");
    assert!(err.contains("@*"), "and the merged form: {err}");
}

#[test]
fn too_many_arms_is_refused_with_the_number_rather_than_truncated() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    run_and_snapshot(&h, &sid, "cache", "r", 5, 10.0);
    let names: Vec<String> = (0..9).map(|_| "cache@r".to_string()).collect();

    let err = h
        .call(&sid, "collectors.document", json!({ "names": names }))
        .expect_err("refused");
    assert!(err.contains("too many arms (9 > 8)"), "{err}");
    assert!(err.contains("in groups"), "and what to do: {err}");
}

#[test]
fn an_unknown_format_names_the_three_that_exist() {
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    let err = h
        .call(
            &sid,
            "collectors.document",
            json!({ "names": ["cache"], "format": "pdf" }),
        )
        .expect_err("refused");
    assert!(err.contains("`md`, `json` or `folded`"), "{err}");
}

#[test]
fn regenerating_with_a_finding_changes_only_the_finding() {
    // V22, and the reason `path` is optional: `finding` arrives after the first
    // read, so regeneration has to be free and lossless.
    let h = harness();
    let sid = h.sessions.create_named("A").unwrap();
    armed(&h, &sid, "cache");
    run_and_snapshot(&h, &sid, "cache", "before", 20, 10.0);
    run_and_snapshot(&h, &sid, "cache", "after", 20, 5.0);

    let names = json!(["cache@before", "cache@after"]);
    let first = h
        .call(&sid, "collectors.document", json!({ "names": names }))
        .unwrap();
    let second = h
        .call(
            &sid,
            "collectors.document",
            json!({ "names": names, "finding": "halved" }),
        )
        .unwrap();

    let (a, b) = (
        first["content"].as_str().unwrap(),
        second["content"].as_str().unwrap(),
    );
    assert!(a.contains("finding: null"));
    assert!(b.contains("finding: \"halved\""));
    assert!(b.contains("**Finding.** halved"));
    // Every section survives regeneration; the annotation is not traded for
    // anything.
    for section in ["## 1.", "## 2.", "## 3.", "## 4.", "## 5."] {
        assert!(a.contains(section) && b.contains(section), "{section}");
    }
}
