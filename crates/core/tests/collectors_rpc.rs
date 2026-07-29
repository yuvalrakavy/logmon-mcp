//! The `collectors.*` and `traces.profile` contract surface (spec §7), plus the
//! §1.1 `traces.slow` repair.
//!
//! Everything goes through `RpcHandler::handle`, so these exercise the wire
//! shapes a client actually sees — parameter parsing, admission, error text —
//! rather than the projection layer, which has its own unit tests.

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
