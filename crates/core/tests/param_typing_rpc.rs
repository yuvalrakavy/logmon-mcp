//! Absent, `null`, and present-but-wrong-typed are three different things.
//!
//! `Value::as_str` and its siblings answer `None` both when a key is absent and
//! when it is present with the wrong type, and every reader in `rpc_handler.rs`
//! used to treat the two identically. The result was not an error message but a
//! *different answer*: a wrong-typed `filter` returned the whole buffer, a
//! wrong-typed `reset` discarded a collector's live window, a wrong-typed
//! `group_keys` armed an ungrouped collector and persisted it.
//!
//! Every test here comes in a pair — one that an absent (or `null`) parameter
//! still takes its default, one that a wrong-typed parameter is refused with a
//! message naming it. The pair is the point: a fix that only rejected would
//! break every caller relying on a default, and a fix that only defaulted would
//! be the bug.

use logmon_broker_core::daemon::domain::{
    Domain, DomainConfig, DomainId, DomainRegistry, DomainSource,
};
use logmon_broker_core::daemon::rpc_handler::{DomainPolicy, RpcHandler};
use logmon_broker_core::daemon::session::{SessionId, SessionRegistry};
use logmon_broker_core::daemon::span_processor::process_span_for_domain;
use logmon_broker_core::domain_data::DomainDataStore;
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use logmon_broker_core::gelf::message::{Level, LogEntry, LogSource};
use logmon_broker_core::receiver::ReceiverMetrics;
use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::span::types::{SpanEntry, SpanKind, SpanStatus};
use logmon_broker_core::store::bookmarks::BookmarkStore;
use logmon_broker_protocol::RpcRequest;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const TRACE_ID: u128 = 7;
const TRACE_HEX: &str = "7";

struct Harness {
    handler: Arc<RpcHandler>,
    domains: Arc<DomainRegistry>,
    sessions: Arc<SessionRegistry>,
    collectors: Arc<logmon_broker_core::collector::registry::CollectorRegistry>,
    pipeline: Arc<LogPipeline>,
    _dir: tempfile::TempDir,
}

fn make_domain(name: &str) -> Arc<Domain> {
    let seq = Arc::new(SeqCounter::new());
    Arc::new(Domain::from_parts(
        DomainConfig {
            name: DomainId::new(name).expect("valid domain name"),
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
    let dir = tempfile::tempdir().expect("tempdir");
    let domains = Arc::new(DomainRegistry::new());
    let default = make_domain("default");
    let pipeline = default.pipeline.clone();
    domains.insert(default);
    domains.insert(make_domain("other"));
    let sessions = Arc::new(SessionRegistry::new());
    let collectors = Arc::new(logmon_broker_core::collector::registry::CollectorRegistry::new());
    let handler = Arc::new(
        RpcHandler::new(
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
        )
        .with_domain_data(Arc::new(DomainDataStore::new(
            dir.path().to_path_buf(),
            "0.9.0".into(),
        ))),
    );
    Harness {
        handler,
        domains,
        sessions,
        collectors,
        pipeline,
        _dir: dir,
    }
}

impl Harness {
    fn call(&self, sid: &SessionId, method: &str, params: Value) -> Result<Value, String> {
        let resp = self
            .handler
            .handle(sid, &RpcRequest::new(1, method, params));
        match resp.error {
            Some(e) => Err(e.message),
            None => Ok(resp.result.unwrap_or(Value::Null)),
        }
    }

    fn ok(&self, sid: &SessionId, method: &str, params: Value) -> Value {
        self.call(sid, method, params.clone())
            .unwrap_or_else(|e| panic!("{method} {params} should have succeeded: {e}"))
    }

    fn err(&self, sid: &SessionId, method: &str, params: Value) -> String {
        match self.call(sid, method, params.clone()) {
            Ok(v) => panic!("{method} {params} should have been refused, got {v}"),
            Err(e) => e,
        }
    }

    fn feed_log(&self, seq: u64, level: Level, msg: &str, trace: Option<u128>) {
        self.pipeline.append_to_store(LogEntry {
            seq,
            timestamp: chrono::Utc::now(),
            level,
            message: msg.to_string(),
            full_message: None,
            host: "test".into(),
            facility: Some("app".into()),
            file: None,
            line: None,
            additional_fields: HashMap::new(),
            trace_id: trace,
            span_id: None,
            matched_filters: Vec::new(),
            source: LogSource::Filter,
        });
    }

    fn feed_span(&self, span: &SpanEntry) {
        let id = DomainId::default_domain();
        let d = self.domains.get(&id).expect("default domain");
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
    let base = chrono::DateTime::from_timestamp_nanos(1_700_000_000_000_000_000);
    SpanEntry {
        seq: 0,
        trace_id: TRACE_ID,
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

/// A session with three logs, three spans, one filter, one trigger, one
/// bookmark and one armed collector — enough that every method under test has
/// something real to answer with, so a refusal is about the parameter and not
/// about an empty daemon.
fn populated() -> (Harness, SessionId) {
    let h = harness();
    let sid = h.sessions.create_named("A").expect("session");
    h.ok(&sid, "filters.add", json!({ "filter": "ALL" }));
    h.ok(&sid, "triggers.add", json!({ "filter": "l>=ERROR" }));
    // Armed BEFORE the spans it must count: a collector measures the window it
    // was present for, so arming after the traffic would leave every `matched`
    // assertion below reading 0 for a reason that has nothing to do with
    // parameters.
    h.ok(
        &sid,
        "collectors.add",
        json!({ "name": "c", "filter": "sv=store_server" }),
    );
    h.feed_log(1, Level::Info, "alpha", Some(TRACE_ID));
    h.feed_log(2, Level::Info, "beta", None);
    h.feed_log(3, Level::Error, "gamma", None);
    h.feed_span(&span("store_server", "reconcile", 10.0));
    h.feed_span(&span("store_server", "commit", 250.0));
    h.feed_span(&span("other_service", "ignored", 500.0));
    // And LAST, so its default `start_seq` sits above both stores' oldest and
    // the sweep every bookmark call runs does not evict it out from under the
    // assertions.
    h.ok(&sid, "bookmarks.add", json!({ "name": "bm" }));
    h.ok(
        &sid,
        "collectors.snapshot",
        json!({ "name": "c", "label": "baseline", "reset": false }),
    );
    (h, sid)
}

// ---------------------------------------------------------------------------
// The reported repro
// ---------------------------------------------------------------------------

/// The defect exactly as it was proven over the socket on 2026-07-31:
///
/// ```text
/// → {"method":"logs.recent","params":{"count":"abc"}}
/// ← {"result":{"count":50,"buffer_total":1103}}      # no error, silently 50
/// ```
#[test]
fn a_stringified_count_is_refused_rather_than_silently_defaulting_to_fifty() {
    let (h, sid) = populated();
    let e = h.err(&sid, "logs.recent", json!({ "count": "abc" }));
    assert!(
        e.contains("`count`") && e.contains("string"),
        "must name the parameter and what arrived: {e}"
    );

    // …and the default is untouched for a caller who simply omitted it.
    let got = h.ok(&sid, "logs.recent", json!({}));
    assert_eq!(
        got["count"], 3,
        "all three logs, the default count being 50"
    );
}

// ---------------------------------------------------------------------------
// Shape 2 — `None` selected different SEMANTICS. The dangerous ones.
// ---------------------------------------------------------------------------

/// A wrong-typed `filter` used to mean **no filter**, so a query meant to
/// narrow the buffer returned all of it — the failure mode that looks like a
/// successful answer.
#[test]
fn a_wrong_typed_filter_is_refused_on_every_query_that_takes_one() {
    let (h, sid) = populated();
    let bad = json!({"expr": "l>=ERROR"});
    for (method, mut params) in [
        ("logs.recent", json!({})),
        ("logs.export", json!({})),
        ("traces.recent", json!({})),
        ("traces.get", json!({ "trace_id": TRACE_HEX })),
        ("traces.slow", json!({})),
        ("traces.logs", json!({ "trace_id": TRACE_HEX })),
        ("traces.profile", json!({})),
        ("filters.edit", json!({ "id": 1 })),
        ("triggers.edit", json!({ "id": 1 })),
        ("collectors.edit", json!({ "name": "c" })),
    ] {
        params["filter"] = bad.clone();
        let e = h.err(&sid, method, params);
        assert!(
            e.contains("`filter`"),
            "{method} must name the parameter: {e}"
        );
    }
}

/// The same filters, omitted, still mean "no filter" — the default this class
/// of fix must not break.
#[test]
fn an_absent_filter_still_means_no_filter() {
    let (h, sid) = populated();
    assert_eq!(h.ok(&sid, "logs.recent", json!({}))["count"], 3);
    assert_eq!(h.ok(&sid, "logs.export", json!({}))["count"], 3);
    assert_eq!(h.ok(&sid, "traces.recent", json!({}))["count"], 1);
    // An explicit `null` is a client serialising an omitted field, not a
    // different request (`domain_data/store.rs:12`).
    assert_eq!(
        h.ok(&sid, "logs.recent", json!({ "filter": null }))["count"],
        3
    );
}

/// `{"filter": …}` narrowing still works — otherwise the test above would pass
/// on a handler that ignored filters entirely.
#[test]
fn a_well_typed_filter_still_narrows() {
    let (h, sid) = populated();
    let got = h.ok(&sid, "logs.recent", json!({ "filter": "l>=ERROR" }));
    assert_eq!(got["count"], 1, "only `gamma` is an error");
}

/// `reset` defaults to **true**, so a wrong-typed value did not merely ignore
/// the caller — it zeroed the collector's live window and reported success.
#[test]
fn a_stringified_reset_does_not_discard_the_live_window() {
    let (h, sid) = populated();
    let before = h.ok(&sid, "collectors.get", json!({ "name": "c" }));
    assert_eq!(
        before["matched"], 2,
        "two store_server spans are in the window"
    );

    let e = h.err(
        &sid,
        "collectors.snapshot",
        json!({ "name": "c", "reset": "false" }),
    );
    assert!(
        e.contains("`reset`") && e.contains("false"),
        "a stringified boolean is the common mistake; say so: {e}"
    );

    let after = h.ok(&sid, "collectors.get", json!({ "name": "c" }));
    assert_eq!(
        after["matched"], 2,
        "the refused call must not have taken the window with it"
    );
}

/// The other half: an omitted `reset` still resets, because a snapshot's whole
/// job is to end one run and start the next in a single call.
#[test]
fn an_absent_reset_still_resets_and_an_explicit_false_still_keeps() {
    let (h, sid) = populated();
    h.ok(
        &sid,
        "collectors.snapshot",
        json!({ "name": "c", "label": "a" }),
    );
    assert_eq!(
        h.ok(&sid, "collectors.get", json!({ "name": "c" }))["matched"],
        0,
        "the default is reset"
    );

    h.feed_span(&span("store_server", "reconcile", 10.0));
    h.ok(
        &sid,
        "collectors.snapshot",
        json!({ "name": "c", "label": "b", "reset": false }),
    );
    assert_eq!(
        h.ok(&sid, "collectors.get", json!({ "name": "c" }))["matched"],
        1,
        "an explicit false still keeps the window"
    );
}

/// `per_name` / `per_group` / `projections` share `reset`'s shape: absent means
/// `true`, so a wrong-typed value silently recorded a policy nobody chose.
#[test]
fn the_snapshot_policy_flags_are_refused_when_wrong_typed_and_default_true_when_absent() {
    let (h, sid) = populated();
    for key in ["per_name", "per_group", "projections"] {
        let e = h.err(
            &sid,
            "collectors.snapshot",
            json!({ "name": "c", key: "true", "reset": false }),
        );
        assert!(e.contains(&format!("`{key}`")), "must name {key}: {e}");
    }
    // Absent → all three on, which is what makes `sampled` present below.
    let snap = h.ok(
        &sid,
        "collectors.snapshot",
        json!({ "name": "c", "label": "defaults", "reset": false }),
    );
    assert!(
        !snap["sampled"].is_null(),
        "projections default to on: {snap}"
    );
}

/// A wrong-typed `group_keys` used to arm an **ungrouped** collector — and
/// persist it, holding a 64 MiB slice of a reservation that fits about four.
#[test]
fn a_wrong_typed_group_keys_does_not_arm_an_ungrouped_collector() {
    let (h, sid) = populated();
    let e = h.err(
        &sid,
        "collectors.add",
        json!({ "name": "g", "filter": "ALL", "group_keys": "service" }),
    );
    assert!(e.contains("`group_keys`"), "must name the parameter: {e}");
    assert!(
        h.call(&sid, "collectors.get", json!({ "name": "g" }))
            .is_err(),
        "nothing may have been armed by a refused call"
    );

    // A non-string ELEMENT was silently dropped, so `["a", 7]` armed a
    // collector grouped by one key when two were asked for.
    let e = h.err(
        &sid,
        "collectors.add",
        json!({ "name": "g", "filter": "ALL", "group_keys": ["service", 7] }),
    );
    assert!(e.contains("group_keys[1]"), "must name which element: {e}");
}

/// The three states `group_keys` has to keep apart on an edit: absent (leave
/// alone), an explicit empty array (a structural change, which discards the
/// live window by design), and a wrong type (refused, discarding nothing).
#[test]
fn group_keys_on_edit_keeps_absent_empty_and_wrong_typed_apart() {
    let (h, sid) = populated();
    h.ok(
        &sid,
        "collectors.add",
        json!({ "name": "g", "filter": "ALL", "group_keys": ["service"] }),
    );
    h.feed_span(&span("store_server", "reconcile", 10.0));
    let matched =
        |h: &Harness| h.ok(&sid, "collectors.get", json!({ "name": "g" }))["matched"].clone();
    assert_eq!(matched(&h), json!(1));

    // Wrong type: refused, and the window survives.
    let e = h.err(
        &sid,
        "collectors.edit",
        json!({ "name": "g", "group_keys": 7 }),
    );
    assert!(e.contains("`group_keys`"), "must name the parameter: {e}");
    assert_eq!(matched(&h), json!(1), "a refused edit discards nothing");

    // Absent: `null` reads as absent, so this edit changes only the
    // description and leaves both the keys and the window alone.
    let out = h.ok(
        &sid,
        "collectors.edit",
        json!({ "name": "g", "group_keys": null, "description": "still grouped" }),
    );
    assert_eq!(out["zeroed"], false, "null is absent, not a change: {out}");
    assert_eq!(out["group_keys"], json!(["service"]));
    assert_eq!(matched(&h), json!(1));

    // An explicit empty array remains a legal, structural change.
    let out = h.ok(
        &sid,
        "collectors.edit",
        json!({ "name": "g", "group_keys": [] }),
    );
    assert_eq!(out["group_keys"], json!([]));
    assert_eq!(
        out["zeroed"], true,
        "clearing the keys is structural: {out}"
    );
}

/// `level` absent means `Level::Tree`, the most expensive tier — so a
/// wrong-typed value silently bought the most costly recording policy.
#[test]
fn a_wrong_typed_level_is_refused_and_an_absent_one_is_still_tree() {
    let (h, sid) = populated();
    let e = h.err(
        &sid,
        "collectors.add",
        json!({ "name": "l", "filter": "ALL", "level": 3 }),
    );
    assert!(e.contains("`level`"), "must name the parameter: {e}");

    let armed = h.ok(
        &sid,
        "collectors.add",
        json!({ "name": "l", "filter": "ALL" }),
    );
    assert_eq!(armed["level"], "tree", "absent is still the default tier");
}

/// A wrong-typed `domain` on an edit left the pin exactly where it was, so a
/// re-pin reported success against the old domain.
#[test]
fn a_wrong_typed_domain_does_not_silently_leave_the_pin() {
    let (h, sid) = populated();
    let e = h.err(
        &sid,
        "collectors.edit",
        json!({ "name": "c", "domain": ["other"] }),
    );
    assert!(e.contains("`domain`"), "must name the parameter: {e}");
    assert_eq!(
        h.ok(&sid, "collectors.list", json!({}))["collectors"][0]["domain"],
        "default"
    );

    // A re-pin is only legal on a zeroed collector, so the window goes first —
    // that rule is what makes the refusal above matter: the caller had to give
    // up a run to make this call, and getting the old pin back for it would be
    // the expensive version of the silent wrong answer.
    h.ok(&sid, "collectors.reset", json!({ "name": "c" }));
    let out = h.ok(
        &sid,
        "collectors.edit",
        json!({ "name": "c", "domain": "other" }),
    );
    assert_eq!(out["domain"], "other", "a well-typed re-pin still works");
}

/// A wrong-typed `snapshot` label read the LIVE window instead of the recorded
/// run — the same shape as the read the caller asked for, answering a
/// different question.
#[test]
fn a_wrong_typed_snapshot_label_does_not_fall_through_to_the_live_window() {
    let (h, sid) = populated();
    let e = h.err(
        &sid,
        "collectors.get",
        json!({ "name": "c", "snapshot": 1 }),
    );
    assert!(e.contains("`snapshot`"), "must name the parameter: {e}");

    let live = h.ok(&sid, "collectors.get", json!({ "name": "c" }));
    assert!(
        live["snapshot"].is_null(),
        "absent still reads the live window"
    );
    let recorded = h.ok(
        &sid,
        "collectors.get",
        json!({ "name": "c", "snapshot": "baseline" }),
    );
    assert_eq!(recorded["snapshot"], "baseline");
}

/// `bookmarks.clear` defaults `session` to the CALLER, so a wrong-typed value
/// cleared the caller's own bookmarks in place of the ones they named.
#[test]
fn a_wrong_typed_session_does_not_clear_the_callers_own_bookmarks() {
    let (h, sid) = populated();
    let e = h.err(&sid, "bookmarks.clear", json!({ "session": 7 }));
    assert!(e.contains("`session`"), "must name the parameter: {e}");
    assert_eq!(
        h.ok(&sid, "bookmarks.list", json!({}))["count"],
        1,
        "a refused clear must not have taken the caller's bookmark"
    );

    let cleared = h.ok(&sid, "bookmarks.clear", json!({}));
    assert_eq!(cleared["removed_count"], 1, "absent still means the caller");
}

/// A wrong-typed `session` on a LIST silently widened it to every session.
#[test]
fn a_wrong_typed_session_filter_is_refused_on_bookmarks_list() {
    let (h, sid) = populated();
    let e = h.err(&sid, "bookmarks.list", json!({ "session": 7 }));
    assert!(e.contains("`session`"), "must name the parameter: {e}");
    assert_eq!(h.ok(&sid, "bookmarks.list", json!({}))["count"], 1);
}

/// A wrong-typed `trace_id` on `logs.recent` fell out of the index-lookup
/// shortcut into a full buffer scan — a different query, reported as success.
#[test]
fn a_wrong_typed_trace_id_does_not_fall_through_to_a_buffer_scan() {
    let (h, sid) = populated();
    let e = h.err(&sid, "logs.recent", json!({ "trace_id": 7 }));
    assert!(e.contains("`trace_id`"), "must name the parameter: {e}");
    let got = h.ok(&sid, "logs.recent", json!({ "trace_id": TRACE_HEX }));
    assert_eq!(got["count"], 1, "only the trace-linked log");
}

/// `group_by`, `format` and the projection options each pick semantics from
/// `None`: the ungrouped shape, markdown, no warm-up skip.
#[test]
fn the_read_shaping_options_are_refused_when_wrong_typed() {
    let (h, sid) = populated();
    for (method, mut params, key) in [
        ("traces.slow", json!({}), "group_by"),
        ("traces.profile", json!({}), "group_by"),
        ("collectors.get", json!({ "name": "c" }), "group_by"),
        ("collectors.diff", json!({ "a": "c", "b": "c" }), "group_by"),
        ("collectors.document", json!({ "names": ["c"] }), "group_by"),
        ("collectors.document", json!({ "names": ["c"] }), "format"),
        ("traces.profile", json!({}), "skip_warmup_ms"),
        ("collectors.get", json!({ "name": "c" }), "top_n"),
        ("collectors.history", json!({ "name": "c" }), "limit"),
        ("collectors.history", json!({ "name": "c" }), "merge"),
        (
            "collectors.diff",
            json!({ "a": "c", "b": "c" }),
            "allow_lossy",
        ),
        ("traces.slow", json!({}), "min_duration_ms"),
        (
            "traces.get",
            json!({ "trace_id": TRACE_HEX }),
            "include_logs",
        ),
        ("collectors.document", json!({ "names": ["c"] }), "finding"),
        (
            "collectors.add",
            json!({"name":"x","filter":"ALL"}),
            "max_sample_bytes",
        ),
        (
            "collectors.add",
            json!({"name":"x","filter":"ALL"}),
            "description",
        ),
        ("bookmarks.add", json!({ "name": "b2" }), "replace"),
        ("bookmarks.add", json!({ "name": "b2" }), "start_seq"),
        ("triggers.add", json!({ "filter": "ALL" }), "oneshot"),
        ("domain_data.get", json!({}), "prefix"),
        ("domain_data.get", json!({}), "validated_before_secs"),
    ] {
        // `{}` is the wrong type for every one of these — string, number,
        // boolean and array alike — so one probe covers the whole table.
        params[key] = json!({});
        let e = h.err(&sid, method, params);
        assert!(
            e.contains(&format!("`{key}`")),
            "{method} must name `{key}`: {e}"
        );
    }
}

/// The shape logmon's OWN MCP shim puts on the wire, and the one defect in this
/// class that was already firing in production rather than waiting for a
/// malformed client.
///
/// `edit_collector` builds its params with `json!({… "group_keys": p.group_keys
/// …})` (`crates/mcp/src/server.rs:939`), so an agent editing only the
/// description sent `group_keys: null`. `params.get("group_keys")` answered
/// `Some(Value::Null)` — *present* — and the old presence-check took that
/// branch, read the null as an empty array, and handed the registry
/// `Some([])`. Against a collector that HAD group keys that is a structural
/// change (`collector/registry.rs:204`): the keys were silently cleared and the
/// live window discarded, on an edit that only renamed the thing.
#[test]
fn the_shims_own_all_nulls_shape_neither_errors_nor_destroys() {
    let (h, sid) = populated();
    h.ok(
        &sid,
        "collectors.add",
        json!({ "name": "g", "filter": "ALL", "group_keys": ["service"] }),
    );
    h.feed_span(&span("store_server", "reconcile", 10.0));

    let out = h.ok(
        &sid,
        "collectors.edit",
        json!({
            "name": "g",
            "description": "renamed only",
            "filter": null,
            "level": null,
            "group_keys": null,
            "max_sample_bytes": null,
            "domain": null,
        }),
    );
    assert_eq!(
        out["group_keys"],
        json!(["service"]),
        "a null the shim sends for an untouched field must not clear it: {out}"
    );
    assert_eq!(out["zeroed"], false, "…nor discard the window: {out}");
    assert_eq!(
        h.ok(&sid, "collectors.get", json!({ "name": "g" }))["matched"],
        1
    );

    // The rest of the shim's surface, in the shape it actually sends.
    h.ok(
        &sid,
        "logs.recent",
        json!({ "count": null, "filter": null, "trace_id": null }),
    );
    h.ok(
        &sid,
        "logs.context",
        json!({ "seq": 2, "before": null, "after": null }),
    );
    h.ok(
        &sid,
        "logs.export",
        json!({ "count": null, "filter": null }),
    );
    h.ok(
        &sid,
        "traces.recent",
        json!({ "count": null, "filter": null }),
    );
    h.ok(
        &sid,
        "traces.slow",
        json!({ "min_duration_ms": null, "count": null, "filter": null, "group_by": null }),
    );
    h.ok(
        &sid,
        "collectors.add",
        json!({
            "name": "shim", "filter": "ALL", "level": null, "group_keys": null,
            "description": null, "max_sample_bytes": null, "threshold": null,
        }),
    );
    h.ok(
        &sid,
        "collectors.snapshot",
        json!({
            "name": "shim", "label": null, "description": null, "meta": null,
            "reset": null, "projections": null,
        }),
    );
    h.ok(
        &sid,
        "collectors.history",
        json!({ "name": "shim", "limit": null, "merge": null }),
    );
}

// ---------------------------------------------------------------------------
// Shape 3 — the message said "missing" for a value that was present
// ---------------------------------------------------------------------------

#[test]
fn a_present_but_wrong_typed_required_parameter_is_not_reported_as_missing() {
    let (h, sid) = populated();
    for (method, key, bad) in [
        ("logs.context", "seq", json!("1")),
        ("spans.context", "seq", json!("1")),
        ("filters.add", "filter", json!(1)),
        ("filters.edit", "id", json!("1")),
        ("filters.remove", "id", json!("1")),
        ("triggers.add", "filter", json!(1)),
        ("triggers.edit", "id", json!("1")),
        ("triggers.remove", "id", json!("1")),
        ("traces.get", "trace_id", json!(7)),
        ("traces.summary", "trace_id", json!(7)),
        ("traces.logs", "trace_id", json!(7)),
        ("sessions.drop", "name", json!(7)),
        ("bookmarks.add", "name", json!(7)),
        ("bookmarks.remove", "name", json!(7)),
        ("collectors.add", "name", json!(7)),
        ("collectors.get", "name", json!(7)),
        ("collectors.edit", "name", json!(7)),
        ("collectors.snapshot", "name", json!(7)),
        ("collectors.history", "name", json!(7)),
        ("collectors.reset", "name", json!(7)),
        ("collectors.remove", "name", json!(7)),
        ("collectors.diff", "a", json!(7)),
        ("collectors.document", "names", json!("c")),
        ("domain_data.remove", "patterns", json!("/a")),
    ] {
        let e = h.err(&sid, method, json!({ key: bad })); // no other params needed
        assert!(
            e.contains(&format!("`{key}`")) || e.contains(key),
            "{method} must name `{key}`: {e}"
        );
        assert!(
            !e.contains("missing required parameter"),
            "{method}.{key} was PRESENT — reporting it missing sends the caller \
             looking for a key they already sent: {e}"
        );
    }
}

/// Inside a batch the same rule needs an index, or a caller with thirty
/// entries learns only that one of them was wrong.
#[test]
fn a_batch_entry_names_its_index_and_says_which_of_missing_or_wrong_typed() {
    let (h, sid) = populated();
    let e = h.err(
        &sid,
        "domain_data.update",
        json!({ "entries": [{"path": "/a", "value": "1"}, {"path": 7}] }),
    );
    assert!(
        e.contains("entries[1].path") && e.contains("not a number"),
        "must name the index and what arrived: {e}"
    );

    let e = h.err(
        &sid,
        "domain_data.update",
        json!({ "entries": [{"value": "1"}] }),
    );
    assert!(
        e.contains("entries[0]") && e.contains("missing"),
        "a genuinely absent path is missing, not mistyped: {e}"
    );

    // A well-formed batch is untouched by either rule.
    let out = h.ok(
        &sid,
        "domain_data.update",
        json!({ "entries": [{"path": "/Build/commit", "value": "9f2a1c4"}] }),
    );
    assert_eq!(out["outcomes"][0]["outcome"], "created");
}

/// The other half: a genuinely absent required parameter — and an explicit
/// `null`, which is absence spelled differently — still reports missing.
#[test]
fn an_absent_required_parameter_still_reports_missing() {
    let (h, sid) = populated();
    for (method, key) in [
        ("logs.context", "seq"),
        ("filters.edit", "id"),
        ("traces.get", "trace_id"),
        ("collectors.get", "name"),
        ("bookmarks.remove", "name"),
    ] {
        for params in [json!({}), json!({ key: null })] {
            let e = h.err(&sid, method, params.clone());
            assert!(
                e.contains(&format!("missing required parameter: {key}")),
                "{method} {params} must report {key} missing: {e}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Range and truncation — the same root, one cast further down
// ---------------------------------------------------------------------------

/// `as u32` wrapped: `pre_window: 4294967296` became **0** — the largest
/// window a caller could ask for, capturing nothing, reported as success.
#[test]
fn a_window_that_does_not_fit_in_u32_is_refused_rather_than_truncated() {
    let (h, sid) = populated();
    for key in ["pre_window", "post_window", "notify_context"] {
        let e = h.err(
            &sid,
            "triggers.add",
            json!({ "filter": "ALL", key: 4_294_967_296u64 }),
        );
        assert!(
            e.contains(&format!("`{key}`")) && e.contains("4294967295"),
            "must name the parameter and the bound: {e}"
        );
    }
    // In range, including the explicit 0 that §6 decision #4 promises is honored.
    let added = h.ok(
        &sid,
        "triggers.add",
        json!({ "filter": "ALL", "pre_window": 0, "post_window": 4_294_967_295u64 }),
    );
    let listed = h.ok(&sid, "triggers.list", json!({}));
    let t = listed["triggers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == added["id"])
        .expect("the trigger just added");
    assert_eq!(t["pre_window"], 0, "an explicit 0 is not an absent value");
    assert_eq!(t["post_window"], 4_294_967_295u64);
}

/// `id: 4294967297` addressed filter **1**.
#[test]
fn an_id_that_does_not_fit_in_u32_addresses_nothing() {
    let (h, sid) = populated();
    for (method, params) in [
        ("filters.remove", json!({ "id": 4_294_967_297u64 })),
        ("triggers.remove", json!({ "id": 4_294_967_297u64 })),
        (
            "filters.edit",
            json!({ "id": 4_294_967_297u64, "filter": "ALL" }),
        ),
    ] {
        let e = h.err(&sid, method, params);
        assert!(e.contains("`id`"), "{method} must name the parameter: {e}");
    }
    // Filter 1 is still there — it was never the one addressed.
    assert_eq!(h.ok(&sid, "filters.list", json!({}))["filters"][0]["id"], 1);
}

/// `context_by_seq` computes `idx + after + 1` (`store/memory.rs:161`,
/// `span/store.rs:221`). With no `[profile.release]` overflow-checks setting
/// that wraps in release and panics in debug — inside the connection task, so
/// under the previous code this test aborted the process rather than failing.
#[test]
fn a_context_window_larger_than_any_buffer_is_refused_rather_than_overflowing() {
    let (h, sid) = populated();
    for method in ["logs.context", "spans.context"] {
        for key in ["before", "after"] {
            let e = h.err(&sid, method, json!({ "seq": 1, key: u64::MAX }));
            assert!(
                e.contains(&format!("`{key}`")) && e.contains("10000000"),
                "{method} must name the parameter and the bound: {e}"
            );
        }
    }
    // A window that fits still works, and still defaults when absent.
    assert!(h.ok(&sid, "logs.context", json!({ "seq": 2 }))["count"]
        .as_u64()
        .is_some_and(|c| c == 3));
    assert_eq!(
        h.ok(
            &sid,
            "logs.context",
            json!({ "seq": 2, "before": 0, "after": 0 })
        )["count"],
        1,
        "an explicit 0 window is not an absent one"
    );
}

/// `chrono::Duration::seconds` PANICS out of range and `s as i64` turns a large
/// `u64` negative, so this parameter could either move the cutoff into the
/// future or take the connection task down.
#[test]
fn an_unusable_age_in_seconds_is_refused_rather_than_panicking() {
    let (h, sid) = populated();
    let e = h.err(
        &sid,
        "domain_data.get",
        json!({ "validated_before_secs": u64::MAX }),
    );
    assert!(
        e.contains("`validated_before_secs`"),
        "must name the parameter: {e}"
    );
    // A usable one still filters, and an absent one still means "no cutoff".
    h.ok(
        &sid,
        "domain_data.get",
        json!({ "validated_before_secs": 60 }),
    );
    assert!(
        h.ok(&sid, "domain_data.get", json!({}))["total"]
            .as_u64()
            .is_some_and(|t| t > 0),
        "logmon's own keys are always there"
    );
}

// ---------------------------------------------------------------------------
// Defaults, in one place
// ---------------------------------------------------------------------------

/// Every default this change could have broken, asserted where it is
/// observable. A fix that only rejected wrong types would pass every test
/// above and fail every one of these.
///
/// The `null` half is not a nicety. logmon's own MCP shim builds params with
/// `json!({"count": p.count, …})` over `Option` fields
/// (`crates/mcp/src/server.rs:475`), and `json!` renders `None` as `null`
/// rather than omitting the key — so every call from every agent arrives with
/// a `null` for each parameter it did not set. Refusing `null` as a wrong type
/// would refuse the shim's whole surface on first use.
#[test]
fn absent_and_null_both_still_take_the_default() {
    let (h, sid) = populated();
    for params in [json!({}), json!({ "count": null, "filter": null })] {
        let got = h.ok(&sid, "logs.recent", params.clone());
        assert_eq!(got["count"], 3, "logs.recent default count: {params}");
    }
    assert_eq!(
        h.ok(&sid, "traces.get", json!({ "trace_id": TRACE_HEX }))["log_count"],
        1,
        "include_logs defaults to true"
    );
    assert_eq!(
        h.ok(
            &sid,
            "traces.get",
            json!({ "trace_id": TRACE_HEX, "include_logs": false })
        )["log_count"],
        0,
        "…and an explicit false is still honored"
    );
    assert_eq!(
        h.ok(&sid, "traces.slow", json!({}))["count"],
        2,
        "min_duration_ms defaults to 100.0: two spans clear it"
    );
    assert!(
        h.ok(&sid, "collectors.get", json!({ "name": "c" }))["exact"]["count"]
            .as_u64()
            .is_some(),
        "group_by defaults to none, which is the ungrouped shape"
    );
    let doc = h.ok(&sid, "collectors.document", json!({ "names": ["c"] }));
    assert_eq!(doc["format"], "md", "format defaults to markdown");
    let added = h.ok(&sid, "bookmarks.add", json!({ "name": "b3" }));
    assert!(
        added["seq"].as_u64().is_some(),
        "start_seq defaults to the current seq"
    );
    assert_eq!(
        h.ok(
            &sid,
            "bookmarks.add",
            json!({ "name": "b4", "start_seq": 0 })
        )["seq"],
        0,
        "…and an explicit 0 is not an absent value"
    );
}
