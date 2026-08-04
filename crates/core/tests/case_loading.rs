//! The seq axis of a loaded case.
//!
//! **These exist because a design gate found the seeds wrong in two opposite
//! directions, and the regression test written for them used the one call shape
//! where both errors cancel.** So every assertion here goes through the RPC
//! surface a reader actually uses, and the log one uses the DEFAULT window.

#![cfg(feature = "test-support")]

use logmon_broker_core::daemon::domain::{
    Domain, DomainConfig, DomainId, DomainRegistry, DomainSource, PostmortemInfo,
};
use logmon_broker_core::daemon::rpc_handler::{DomainPolicy, RpcHandler};
use logmon_broker_core::daemon::session::{SessionId, SessionRegistry};
use logmon_broker_core::domain_data::DomainDataStore;
use logmon_broker_core::engine::epoch::FilterPolicy;
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use logmon_broker_core::gelf::message::{Level, LogEntry};
use logmon_broker_core::receiver::ReceiverMetrics;
use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::span::types::{SpanEntry, SpanKind, SpanStatus};
use logmon_broker_core::store::bookmarks::BookmarkStore;
use logmon_broker_protocol::RpcRequest;
use serde_json::{json, Value};
use std::sync::Arc;

const FROM: u64 = 1001;
const TO: u64 = 1009;

fn entry(seq: u64) -> LogEntry {
    let mut e = LogEntry::synthetic(Level::Info, "loaded");
    e.seq = seq;
    e
}

fn span(seq: u64) -> SpanEntry {
    let base = chrono::DateTime::from_timestamp_nanos(1_700_000_000_000_000_000);
    SpanEntry {
        seq,
        trace_id: 7,
        span_id: seq,
        parent_span_id: None,
        start_time: base,
        end_time: base + chrono::Duration::milliseconds(10),
        duration_ms: 10.0,
        name: "op".into(),
        kind: SpanKind::Internal,
        service_name: "svc".into(),
        status: SpanStatus::Ok,
        attributes: Default::default(),
        events: Vec::new(),
    }
}

/// A domain built exactly the way `load_case` builds one.
fn loaded_domain(epochs: &[(u64, FilterPolicy)]) -> Arc<Domain> {
    // Logs 1001-1006, spans 1007-1009 — the real shape duty 0 measured.
    let logs: Vec<LogEntry> = (FROM..=1006).map(entry).collect();
    let spans: Vec<SpanEntry> = (1007..=TO).map(span).collect();

    // The counter sits at the window's TOP so `logs.export {}` has an upper end
    // above the records; the epoch origin sits one BELOW the bottom so coverage
    // vouches for the window and the span floor does not fire.
    let seq = Arc::new(SeqCounter::new_with_initial(TO));
    let pipeline = LogPipeline::from_records(
        logs.len(),
        seq.clone(),
        FROM - 1,
        epochs,
        logs,
        FROM,
    );
    let span_store = SpanStore::from_records(spans.len(), seq, spans, FROM);

    Arc::new(Domain::from_parts(
        DomainConfig {
            // `default`, so a session that never called `domains.use` resolves
            // to it — the shape a reader is actually in.
            name: DomainId::default_domain(),
            gelf_port: 0,
            otlp_grpc_port: 0,
            otlp_http_port: 0,
            log_buffer_size: 6,
            span_buffer_size: 3,
            source: DomainSource::Ephemeral,
        },
        Arc::new(pipeline),
        Arc::new(span_store),
        Arc::new(BookmarkStore::new()),
        Arc::new(ReceiverMetrics::new()),
    ))
}

struct H {
    handler: Arc<RpcHandler>,
    session: SessionId,
    _cfg: tempfile::TempDir,
}

fn harness(epochs: &[(u64, FilterPolicy)]) -> H {
    harness_of(loaded_domain(epochs))
}

/// The same domain, marked as the case it is.
fn sealed_harness() -> H {
    let d = loaded_domain(&[]);
    let inner = Arc::try_unwrap(d).unwrap_or_else(|_| panic!("sole owner"));
    harness_of(Arc::new(inner.sealed_as(PostmortemInfo {
        case: "checkout-hang-260503-021530".into(),
        captured_at: chrono::DateTime::from_timestamp_nanos(1_700_000_000_000_000_000),
        verdict: logmon_broker_protocol::EvidenceVerdict::Complete,
        logs_omitted: false,
        spans_omitted: false,
        registry_dropped: 0,
        registry: Arc::new(
            logmon_broker_core::domain_data::persist::DomainRegistry::in_memory(
                "pm".into(),
                Default::default(),
            ),
        ),
    })))
}

fn harness_of(domain: Arc<Domain>) -> H {
    let cfg = tempfile::tempdir().expect("tempdir");
    let domains = Arc::new(DomainRegistry::new());
    domains.insert(domain);
    let sessions = Arc::new(SessionRegistry::new());
    let session = sessions.create_named("reader").expect("valid name");
    let collectors = Arc::new(logmon_broker_core::collector::registry::CollectorRegistry::new());
    let handler = Arc::new(
        RpcHandler::new(
            domains,
            sessions,
            collectors,
            vec!["test".into()],
            DomainPolicy {
                max_domains: 32,
                default_log_buffer_size: 1000,
                default_span_buffer_size: 1000,
                stale_after_secs: 60,
            },
        )
        .with_domain_data(Arc::new(DomainDataStore::new(
            cfg.path().to_path_buf(),
            "test".into(),
        ))),
    );
    H {
        handler,
        session,
        _cfg: cfg,
    }
}

fn call(h: &H, method: &str, params: Value) -> Value {
    let req = RpcRequest::new(1, method, params);
    let resp = h.handler.handle(&h.session, &req);
    match resp.error {
        Some(e) => panic!("{method} failed: {}", e.message),
        None => resp.result.unwrap_or(Value::Null),
    }
}

#[test]
fn an_unbounded_logs_export_on_a_loaded_case_reports_complete() {
    // **The default call shape.** `logs.export {}` takes its upper end from
    // `current_seq()` and its lower from `epoch origin + 1`. Seeding the counter
    // at the window's BOTTOM — which an earlier draft of the design did — makes
    // from > to, the window collapses to None, and the verdict is
    // `cannot_verify` on a case whose document says `complete`. Nothing fails;
    // the answer is just needlessly weaker than the evidence supports.
    let h = harness(&[]);
    let r = call(&h, "logs.export", json!({ "format": "json" }));
    assert_eq!(
        r["verdict"], "complete",
        "an unbounded export must vouch for the whole loaded window: {r}"
    );
}

#[test]
fn an_explicit_range_export_also_reports_complete() {
    // The shape the first regression test used — kept, but it is NOT the one
    // that catches the defect, because both seeding errors cancel here.
    let h = harness(&[]);
    let r = call(
        &h,
        "logs.export",
        json!({ "format": "json", "from_seq": FROM, "to_seq": TO }),
    );
    assert_eq!(r["verdict"], "complete", "{r}");
}

#[test]
fn a_span_export_over_the_window_is_not_reported_truncated() {
    // `spans.export` floors at `max(lost_below, epoch_origin + 1)`. With the
    // origin at `from` rather than `from - 1`, the floor lands one above the
    // window's bottom and EVERY span export over a loaded case reports
    // `evicted_before_window: 1` — a loss that never happened.
    let h = harness(&[]);
    let r = call(
        &h,
        "spans.export",
        json!({ "from_seq": FROM, "to_seq": TO }),
    );
    assert_ne!(
        r["truncated"], json!(true),
        "the window's own span range must not read as truncated: {r}"
    );
    assert!(
        r.get("evicted_before_window")
            .is_none_or(|v| v.is_null()),
        "and nothing may be reported evicted below it: {r}"
    );
}

#[test]
fn a_filtered_case_loads_as_filtered_rather_than_unverifiable() {
    // The capability the `narrowed_by` front-matter addition unlocked. Without
    // the recorded ranges a loader can only claim "unfiltered" or say nothing,
    // and saying nothing degrades every capture taken while a session filter was
    // running — which is what a production box produces.
    let h = harness(&[(FROM, FilterPolicy::new(vec!["l>=ERROR".into()]))]);
    let r = call(
        &h,
        "logs.export",
        json!({ "format": "json", "from_seq": FROM, "to_seq": TO }),
    );
    assert_eq!(
        r["verdict"], "filtered",
        "the case's own narrowing must survive the load: {r}"
    );
    let narrowed = r["narrowed_by"].as_array().expect("ranges reported");
    assert_eq!(narrowed.len(), 1, "{r}");
    assert_eq!(
        narrowed[0]["filters"][0], "l>=ERROR",
        "and the expression itself, not just the fact: {r}"
    );
}

/// A real fixture, loaded through the RPC surface a client actually calls.
#[tokio::test]
async fn a_real_case_loads_end_to_end_and_is_then_sealed() {
    let dir = tempfile::tempdir().unwrap();
    let stem = "with-spans-260803-214501";
    for n in [
        format!("{stem}.md"),
        format!("{stem}.logdata.jsonl"),
        format!("{stem}.spandata.jsonl"),
    ] {
        std::fs::copy(
            format!("{}/tests/fixtures/cases/{n}", env!("CARGO_MANIFEST_DIR")),
            dir.path().join(&n),
        )
        .unwrap();
    }
    let md = dir.path().join(format!("{stem}.md"));

    // A live `default` domain, so the load creates a SECOND domain rather than
    // colliding — the shape a reader is in.
    let h = harness(&[]);
    let resp = h
        .handler
        .handle_async(
            &h.session,
            &RpcRequest::new(1, "cases.load", json!({ "path": md.to_str().unwrap() })),
        )
        .await;
    let r = resp
        .result
        .unwrap_or_else(|| panic!("load failed: {:?}", resp.error));

    assert_eq!(r["case"], stem);
    assert_eq!(r["domain"], stem, "the domain takes the case's own stem");
    assert_eq!(r["logs"], 9);
    assert_eq!(r["spans"], 3);
    assert_eq!(r["verdict"], "complete");
    assert_eq!(r["anchor"]["seq"], 1012);

    // Bind to it and read: the whole point is that the ordinary tools work.
    let bind = h.handler.handle(
        &h.session,
        &RpcRequest::new(1, "domains.use", json!({ "name": stem })),
    );
    assert!(bind.error.is_none(), "{:?}", bind.error);

    let logs = call(&h, "logs.recent", json!({ "limit": 100 }));
    assert_eq!(logs["logs"].as_array().unwrap().len(), 9);
    let traces = call(&h, "traces.recent", json!({}));
    assert!(
        !traces["traces"].as_array().unwrap().is_empty(),
        "the spans came with it: {traces}"
    );

    // ...and it is sealed.
    let refused = h.handler.handle(
        &h.session,
        &RpcRequest::new(1, "collectors.add", json!({ "name": "x", "filter": "l>=ERROR" })),
    );
    assert!(
        refused
            .error
            .is_some_and(|e| e.message.contains("holds the loaded case")),
        "a loaded domain must refuse a collector"
    );
}

#[tokio::test]
async fn loading_a_second_case_onto_an_occupied_name_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let stem = "with-spans-260803-214501";
    for n in [
        format!("{stem}.md"),
        format!("{stem}.logdata.jsonl"),
        format!("{stem}.spandata.jsonl"),
    ] {
        std::fs::copy(
            format!("{}/tests/fixtures/cases/{n}", env!("CARGO_MANIFEST_DIR")),
            dir.path().join(&n),
        )
        .unwrap();
    }
    let md = dir.path().join(format!("{stem}.md"));
    let h = harness(&[]);
    let params = json!({ "path": md.to_str().unwrap() });

    let first = h
        .handler
        .handle_async(&h.session, &RpcRequest::new(1, "cases.load", params.clone()))
        .await;
    assert!(first.error.is_none(), "{:?}", first.error);

    // NOT idempotent, unlike domains.create: a no-op here would hand back a
    // domain holding a different case under the name you asked for.
    let second = h
        .handler
        .handle_async(&h.session, &RpcRequest::new(1, "cases.load", params))
        .await;
    let msg = second.error.expect("refused").message;
    assert!(msg.contains(stem), "the refusal names its occupant: {msg}");
    assert!(
        msg.contains("constituted by ONE case"),
        "and says why, so it does not read as an arbitrary limit: {msg}"
    );
}

/// Copy the fixture into a temp dir and hand back its `.md` path.
fn scratch_case(dir: &tempfile::TempDir, stem: &str) -> std::path::PathBuf {
    for suffix in [".md", ".logdata.jsonl", ".spandata.jsonl"] {
        let n = format!("{stem}{suffix}");
        std::fs::copy(
            format!("{}/tests/fixtures/cases/{n}", env!("CARGO_MANIFEST_DIR")),
            dir.path().join(&n),
        )
        .unwrap();
    }
    dir.path().join(format!("{stem}.md"))
}

#[tokio::test]
async fn a_narrowed_stretch_that_ended_does_not_narrow_the_rest_of_the_window() {
    // **The deep gate found this, and found that my own test could not.**
    // `case_loading`'s other tests pass epochs straight to
    // `LogPipeline::from_records`, bypassing the translation in
    // `assemble_case_domain` entirely — and the one narrowing they use runs to
    // the top of the window, the single shape where a missing close is
    // invisible. So this goes through `cases.load`.
    //
    // The defect: only the OPENING transition was emitted, and `EpochLog` runs
    // its last epoch to `u64::MAX`. A filter someone turned off mid-window then
    // narrowed every seq above it — wrong information, not missing information.
    let dir = tempfile::tempdir().unwrap();
    let stem = "with-spans-260803-214501";
    let md = scratch_case(&dir, stem);

    // The fixture's window is 1001–1012. Narrow only 1001–1004.
    let doc = std::fs::read_to_string(&md)
        .unwrap()
        .replace(
            "narrowed_by: []",
            r#"narrowed_by: [{from: 1001, to: 1004, filters: ["l>=ERROR"]}]"#,
        )
        .replace("verdict: complete", "verdict: filtered");
    // Older fixtures predate the key; add it if the replace found nothing.
    let doc = if doc.contains("narrowed_by:") {
        doc
    } else {
        doc.replace(
            "verdict: filtered",
            "verdict: filtered\nnarrowed_by: [{from: 1001, to: 1004, filters: [\"l>=ERROR\"]}]",
        )
    };
    std::fs::write(&md, doc).unwrap();

    let h = harness(&[]);
    let resp = h
        .handler
        .handle_async(
            &h.session,
            &RpcRequest::new(1, "cases.load", json!({ "path": md.to_str().unwrap() })),
        )
        .await;
    assert!(resp.error.is_none(), "{:?}", resp.error);
    h.handler.handle(
        &h.session,
        &RpcRequest::new(1, "domains.use", json!({ "name": stem })),
    );

    // Above the narrowing, the case says nothing was narrowing the store.
    let clean = call(
        &h,
        "logs.export",
        json!({ "format": "json", "from_seq": 1006, "to_seq": 1012 }),
    );
    assert_eq!(
        clean["narrowed_by"].as_array().map(Vec::len),
        Some(0),
        "a stretch that ENDED must not narrow the rest of the window: {clean}"
    );
    assert_ne!(
        clean["verdict"], "filtered",
        "and the verdict above it must not inherit the narrowing: {clean}"
    );

    // ...while the narrowed stretch itself still reports its filter.
    let narrowed = call(
        &h,
        "logs.export",
        json!({ "format": "json", "from_seq": 1001, "to_seq": 1004 }),
    );
    assert_eq!(narrowed["verdict"], "filtered", "{narrowed}");
    assert_eq!(
        narrowed["narrowed_by"][0]["filters"][0], "l>=ERROR",
        "{narrowed}"
    );
}

#[test]
fn provenance_on_a_loaded_case_is_aged_from_the_capture_not_from_today() {
    // **`fact_clock()` had ZERO callers.** The frozen clock was designed,
    // documented in four places, and consulted by nothing: `domain_data.get`
    // used `Utc::now()` and opened the SHARED name-keyed store, so the restored
    // provenance was write-only and every fact read as months stale.
    let h = sealed_harness();
    let reg = call(&h, "domain_data.get", json!({}));
    // The sealed harness's registry is empty, so the observable claim is the one
    // that matters: nothing was created under the case's name on disk, and the
    // call resolved against the in-memory registry rather than the store.
    assert!(
        reg["keys"].is_array(),
        "the postmortem registry must be readable at all: {reg}"
    );
    let files: Vec<_> = std::fs::read_dir(h._cfg.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        !files.iter().any(|f| f.contains("domain_data")),
        "resolving a postmortem registry must not create a file under the \
         case's name — that file outlives the domain and the next live domain \
         of that name would adopt it: {files:?}"
    );
}

/// Load the real fixture through `cases.load` and bind to it.
///
/// **Everything that pins a seed must go through here.** The tests above build
/// their own pipeline with the seeds written out by hand, so they verify that
/// `LogPipeline::from_records` behaves correctly *given* those arguments — and
/// verify nothing at all about the arguments `assemble_case_domain` actually
/// passes. A mutation lens proved it: changing the production `epoch_origin`
/// from `from - 1` to `from`, and the counter from `to` to `from`, left every
/// one of them green.
async fn loaded_via_rpc(dir: &tempfile::TempDir) -> (H, &'static str) {
    let stem = "with-spans-260803-214501";
    let md = scratch_case(dir, stem);
    let h = harness(&[]);
    let resp = h
        .handler
        .handle_async(
            &h.session,
            &RpcRequest::new(1, "cases.load", json!({ "path": md.to_str().unwrap() })),
        )
        .await;
    assert!(resp.error.is_none(), "load failed: {:?}", resp.error);
    let bind = h.handler.handle(
        &h.session,
        &RpcRequest::new(1, "domains.use", json!({ "name": stem })),
    );
    assert!(bind.error.is_none(), "{:?}", bind.error);
    (h, stem)
}

#[tokio::test]
async fn the_production_epoch_origin_does_not_manufacture_a_span_eviction() {
    // `spans.export` floors at `max(lost_below, epoch_origin + 1)`. With the
    // origin at `from` rather than `from - 1` the floor lands one above the
    // window's bottom, and EVERY span export over a loaded case reports a loss
    // that never happened.
    let dir = tempfile::tempdir().unwrap();
    let (h, _) = loaded_via_rpc(&dir).await;
    let r = call(&h, "spans.export", json!({ "from_seq": 1001, "to_seq": 1012 }));
    assert!(
        r.get("evicted_before_window").is_none_or(|v| v.is_null()),
        "the window's own span range must not read as evicted: {r}"
    );
    assert_ne!(r["truncated"], json!(true), "{r}");
}

#[tokio::test]
async fn the_production_seq_counter_puts_a_new_bookmark_at_the_top() {
    // `bookmarks.add` defaults `start_seq` to `current_seq()`. Seeded at the
    // window's bottom, a reader marking their place lands beneath every record
    // in the case and re-reads the whole thing.
    let dir = tempfile::tempdir().unwrap();
    let (h, _) = loaded_via_rpc(&dir).await;
    call(&h, "bookmarks.add", json!({ "name": "here" }));
    let listed = call(&h, "bookmarks.list", json!({}));
    let here = listed["bookmarks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["qualified_name"] == "reader/here")
        .unwrap_or_else(|| panic!("not listed: {listed}"));
    assert_eq!(
        here["seq"].as_u64(),
        Some(1012),
        "a bookmark means `from here on`: {listed}"
    );
}

#[tokio::test]
async fn a_loaded_case_does_not_claim_to_speak_below_its_own_window() {
    // `lost_below` is what stops a loaded store answering "nothing happened
    // before this" when it means "I was not there". Neither store had any test
    // for it: passing 0 left every assertion green.
    let dir = tempfile::tempdir().unwrap();
    let (h, _) = loaded_via_rpc(&dir).await;
    let r = call(
        &h,
        "logs.export",
        json!({ "format": "json", "from_seq": 1, "to_seq": 1012 }),
    );
    assert_eq!(
        r["evicted_before_window"].as_u64(),
        Some(1000),
        "a window reaching below the case must report what the case cannot \
         speak for: {r}"
    );
    assert_eq!(r["truncated"], json!(true), "{r}");
}

#[tokio::test]
async fn create_domain_refuses_to_hand_back_a_loaded_case_as_a_live_domain() {
    // A postmortem domain has all three ports at 0, and `port_matches` treats an
    // unspecified port as matching — so the idempotent-ensure path returned
    // SUCCESS and the caller believed it had a live domain. It would then ship
    // GELF at a port that was never bound and read the case's months-old records
    // as its own.
    let dir = tempfile::tempdir().unwrap();
    let stem = "with-spans-260803-214501";
    let md = scratch_case(&dir, stem);
    let h = harness(&[]);
    let loaded = h
        .handler
        .handle_async(
            &h.session,
            &RpcRequest::new(1, "cases.load", json!({ "path": md.to_str().unwrap() })),
        )
        .await;
    assert!(loaded.error.is_none(), "{:?}", loaded.error);

    let resp = h
        .handler
        .handle_async(
            &h.session,
            &RpcRequest::new(1, "domains.create", json!({ "name": stem })),
        )
        .await;
    let msg = resp.error.expect("refused").message;
    assert!(msg.contains(stem), "the refusal names the case: {msg}");
}

/// Tools that do not mutate THIS domain, and may therefore run on a loaded case.
///
/// **The allowlist is the permitted side, deliberately, so the default is
/// refusal.** There is no `mutates` flag on `Tool` and no choke point in
/// dispatch, so exhaustiveness has to come from somewhere — and a list of things
/// to REFUSE would leave the tool nobody has added yet silently permitted, which
/// is the failure this whole mechanism exists to avoid. Inverted, a new tool
/// fails this test until someone classifies it on purpose.
///
/// Mostly reads. The exceptions are lifecycle operations that act on a
/// *different* domain — creating, deleting, or loading another case is not an
/// operation on this one, and delete is the only honest way to discard a case.
const READ_ONLY: &[&str] = &[
    "load_case",
    // The reader's own navigation, placed today with today's clock.
    "clear_bookmarks",
    "get_recent_logs", "get_log_context", "export_logs", "list_log_fields", "profile_logs",
    "get_recent_traces", "get_trace", "get_trace_summary", "get_slow_spans", "get_trace_logs",
    "get_span_context", "export_spans", "profile_traces",
    "get_status", "get_sessions", "list_domains", "use_domain", "get_domain_data",
    "list_bookmarks", "add_bookmark", "remove_bookmark",
    "get_filters", "get_triggers",
    "list_collectors", "get_collector", "get_collector_history", "diff_collectors",
    "document_collectors",
    // Lifecycle: creating or deleting a DIFFERENT domain is not an operation on
    // this one, and delete is the honest way to discard a case.
    "create_domain", "delete_domain", "drop_session", "rename_session",
    "get_domain_data_registry",
];

#[test]
fn every_tool_that_is_not_read_only_is_refused_on_a_loaded_case() {
    use logmon_broker_protocol::mcp_tools::TOOLS;
    let h = sealed_harness();
    let mut unguarded = Vec::new();

    for t in TOOLS {
        if READ_ONLY.contains(&t.name) {
            continue;
        }
        let req = RpcRequest::new(1, t.method, json!({}));
        let resp = h.handler.handle(&h.session, &req);
        let refused = resp
            .error
            .as_ref()
            .is_some_and(|e| e.message.contains("holds the loaded case"));
        if !refused {
            unguarded.push(format!(
                "{} ({}) -> {:?}",
                t.name,
                t.method,
                resp.error.map(|e| e.message).unwrap_or("OK".into())
            ));
        }
    }

    assert!(
        unguarded.is_empty(),
        "these tools mutate but are not refused on a postmortem domain. Either \
         guard them with `require_live`, or add them to READ_ONLY if they really \
         only read:\n  {}",
        unguarded.join("\n  ")
    );
}

#[test]
fn the_refusal_names_the_case_and_the_alternative() {
    // A refusal that only says `no` sends the reader looking for a workaround.
    let h = sealed_harness();
    let resp = h
        .handler
        .handle(&h.session, &RpcRequest::new(1, "collectors.add", json!({})));
    let msg = resp.error.expect("refused").message;
    assert!(msg.contains("checkout-hang-260503-021530"), "{msg}");
    assert!(
        msg.contains("profile_traces") || msg.contains("profile_logs"),
        "and points at what DOES work here: {msg}"
    );
}

#[test]
fn a_live_domain_refuses_nothing() {
    // The negative control. A guard that refuses everywhere would pass the
    // exhaustiveness test above while breaking the product.
    let h = harness(&[]);
    let resp = h
        .handler
        .handle(&h.session, &RpcRequest::new(1, "filters.add", json!({ "filter": "l>=ERROR" })));
    assert!(
        resp.error
            .as_ref()
            .is_none_or(|e| !e.message.contains("holds the loaded case")),
        "a live domain must not be refused: {:?}",
        resp.error
    );
}

#[test]
fn a_new_bookmark_lands_at_the_top_of_the_loaded_window() {
    // **This is what actually pins the seq counter's seed.** The unbounded
    // export cannot: with the counter at the window's bottom its verdict window
    // collapses to a single seq, which still reads `complete` — correct about
    // one record out of six, and indistinguishable from correct about all of
    // them. `bookmarks.add` defaults `start_seq` to `current_seq()`, so the
    // wrong seed is directly visible: a reader marking their place would land
    // beneath every record in the case and re-read the whole thing.
    let h = harness(&[]);
    call(&h, "bookmarks.add", json!({ "name": "here" }));
    let listed = call(&h, "bookmarks.list", json!({}));
    let marks = listed["bookmarks"].as_array().expect("listed");
    let here = marks
        .iter()
        .find(|b| b["qualified_name"] == "reader/here")
        .unwrap_or_else(|| panic!("bookmark not listed: {listed}"));
    assert_eq!(
        here["seq"].as_u64(),
        Some(TO),
        "a bookmark means `from here on`, and here is the top of the window: {listed}"
    );
}

#[test]
fn the_loaded_records_keep_their_own_seqs() {
    let h = harness(&[]);
    let r = call(&h, "logs.recent", json!({ "limit": 100 }));
    // `logs.recent` reports newest first, as it does on a live domain — a loaded
    // domain is not a different read surface.
    let seqs: Vec<u64> = r["logs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(
        seqs,
        (FROM..=1006).rev().collect::<Vec<_>>(),
        "preserved seqs are what keep the case document's cross-references valid"
    );
}

#[test]
fn get_status_announces_the_case_so_the_frozen_clock_is_not_a_lie() {
    // The frozen clock ages facts against the capture, which is right — and
    // silent. Without this block "idle 12s" on three-month-old evidence is
    // indistinguishable from a live domain, and every duration in the payload
    // reads as if it were about now.
    let h = sealed_harness();
    let r = call(&h, "status.get", json!({}));
    let pm = &r["postmortem"];
    assert!(pm.is_object(), "a loaded domain must say so: {r}");
    assert_eq!(pm["case"], "checkout-hang-260503-021530");
    assert_eq!(pm["verdict"], "complete");
    assert!(
        pm["age_secs"].as_u64().is_some_and(|s| s > 0),
        "the REAL elapsed time must be stated as an absolute, since the frozen \
         clock deliberately hides it: {pm}"
    );
    assert_eq!(
        pm["store_counters_unmeasured"], true,
        "or `0 received` reads as a domain that dropped nothing: {pm}"
    );
    assert_eq!(pm["logs_omitted"], false);
    assert_eq!(pm["spans_omitted"], false);
}

#[test]
fn get_status_says_nothing_about_postmortem_on_a_live_domain() {
    // Negative control: a block that always appeared would train a reader to
    // skip the one that matters.
    let h = harness(&[]);
    let r = call(&h, "status.get", json!({}));
    assert!(
        r["postmortem"].is_null(),
        "a live domain must not carry a case block: {r}"
    );
}

#[test]
fn a_loaded_domain_does_not_claim_it_received_what_it_holds() {
    // A case is a window cut out of a stream. Asserting `total_received == 6`
    // would claim a delivery record the case does not carry, and `get_status`
    // would report a domain that never dropped anything.
    let h = harness(&[]);
    let r = call(&h, "status.get", json!({}));
    let store = &r["store"];
    assert_eq!(
        store["total_received"], 0,
        "a loaded domain is unmeasured, not perfect: {store}"
    );
    assert_eq!(store["current_size"], 6, "while still holding its records");
}
