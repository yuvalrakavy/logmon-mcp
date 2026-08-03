//! `cases.create` over the RPC boundary — case-documents spec §5.
//!
//! Everything goes through `RpcHandler::handle` and lands on a real temporary
//! directory, so these exercise what a client actually gets: the wire shape, the
//! files on disk, and whether the two agree.

use logmon_broker_core::daemon::domain::{
    Domain, DomainConfig, DomainId, DomainRegistry, DomainSource,
};
use logmon_broker_core::daemon::log_processor::process_entry_for_domain;
use logmon_broker_core::daemon::rpc_handler::{DomainPolicy, RpcHandler};
use logmon_broker_core::daemon::session::{SessionId, SessionRegistry};
use logmon_broker_core::domain_data::DomainDataStore;
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use logmon_broker_core::gelf::message::{Level, LogEntry};
use logmon_broker_core::receiver::ReceiverMetrics;
use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::store::bookmarks::BookmarkStore;
use logmon_broker_protocol::RpcRequest;
use serde_json::{json, Value};
use std::sync::Arc;

struct Harness {
    handler: Arc<RpcHandler>,
    domains: Arc<DomainRegistry>,
    sessions: Arc<SessionRegistry>,
    session: SessionId,
    archive: tempfile::TempDir,
    _config: tempfile::TempDir,
}

fn make_domain(name: &str) -> Arc<Domain> {
    make_domain_with(name, 1000)
}

fn make_domain_with(name: &str, log_capacity: usize) -> Arc<Domain> {
    make_domain_sized(name, log_capacity, 1000)
}

/// A domain whose span ring is small enough for a test to overflow.
fn make_domain_spans(name: &str, span_capacity: usize) -> Arc<Domain> {
    make_domain_sized(name, 1000, span_capacity)
}

fn make_domain_sized(name: &str, log_capacity: usize, span_capacity: usize) -> Arc<Domain> {
    let seq = Arc::new(SeqCounter::new());
    Arc::new(Domain::from_parts(
        DomainConfig {
            name: DomainId::new(name).expect("valid domain name"),
            gelf_port: 0,
            otlp_grpc_port: 0,
            otlp_http_port: 0,
            log_buffer_size: log_capacity,
            span_buffer_size: span_capacity,
            source: DomainSource::Config,
        },
        Arc::new(LogPipeline::new_with_seq_counter(log_capacity, seq.clone())),
        Arc::new(SpanStore::new(span_capacity, seq)),
        Arc::new(BookmarkStore::new()),
        Arc::new(ReceiverMetrics::new()),
    ))
}

/// A span the store will stamp with the next shared seq.
fn a_span() -> logmon_broker_core::span::types::SpanEntry {
    use logmon_broker_core::span::types::{SpanEntry, SpanKind, SpanStatus};
    let base = chrono::DateTime::from_timestamp_nanos(1_700_000_000_000_000_000);
    SpanEntry {
        seq: 0, // assigned by the store from the shared counter
        trace_id: 7,
        span_id: 1,
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

fn harness() -> Harness {
    harness_with_log_capacity(1000)
}

fn harness_with_log_capacity(log_capacity: usize) -> Harness {
    let config = tempfile::tempdir().expect("tempdir");
    let archive = tempfile::tempdir().expect("tempdir");
    let domains = Arc::new(DomainRegistry::new());
    domains.insert(make_domain_with("default", log_capacity));
    let sessions = Arc::new(SessionRegistry::new());
    let session = sessions.create_named("capturer").expect("valid name");
    let collectors = Arc::new(logmon_broker_core::collector::registry::CollectorRegistry::new());
    let handler = Arc::new(
        RpcHandler::new(
            domains.clone(),
            sessions.clone(),
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
            config.path().to_path_buf(),
            "0.9.0".into(),
        ))),
    );
    Harness {
        handler,
        domains,
        sessions,
        session,
        archive,
        _config: config,
    }
}

impl Harness {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let req = RpcRequest::new(1, method, params);
        let resp = self.handler.handle(&self.session, &req);
        match resp.error {
            Some(e) => Err(e.message),
            None => Ok(resp.result.unwrap_or(Value::Null)),
        }
    }

    /// Feed a log through the real processor, so storage is decided — and the
    /// epoch recorded — exactly as the daemon does it.
    fn feed(&self, level: Level, message: &str) -> u64 {
        self.feed_traced(level, message, None)
    }

    fn feed_traced(&self, level: Level, message: &str, trace_id: Option<u128>) -> u64 {
        let id = DomainId::default_domain();
        let d = self.domains.get(&id).expect("default exists");
        let mut entry = LogEntry::synthetic(level, message);
        entry.trace_id = trace_id;
        process_entry_for_domain(&mut entry, &d.pipeline, &self.sessions, &id);
        entry.seq
    }

    fn dir(&self) -> String {
        self.archive.path().display().to_string()
    }

    /// A capture with the boring parameters filled in.
    fn capture(&self, extra: Value) -> Result<Value, String> {
        let mut params = json!({
            "reason": "hang at 20/20",
            "dir": self.dir(),
        });
        for (k, v) in extra.as_object().expect("object").iter() {
            params[k] = v.clone();
        }
        self.call("cases.create", params)
    }
}

fn document_of(result: &Value) -> String {
    let path = result["paths"][0].as_str().expect("a document path");
    std::fs::read_to_string(path).expect("the document is on disk")
}

// ---------------------------------------------------------------------------

/// The happy path, first: three files, and the wire agrees with the disk.
#[test]
fn a_capture_writes_three_files_and_reports_what_landed() {
    let h = harness();
    for i in 1..=5 {
        h.feed(Level::Info, &format!("entry {i}"));
    }
    let anchor = h.feed(Level::Info, "the interesting one");
    for i in 1..=5 {
        h.feed(Level::Info, &format!("after {i}"));
    }

    let r = h.capture(json!({ "anchor": { "seq": anchor } })).unwrap();

    let paths = r["paths"].as_array().expect("paths");
    assert_eq!(paths.len(), 3, "{r}");
    for p in paths {
        let p = p.as_str().unwrap();
        assert!(
            std::path::Path::new(p).exists(),
            "`paths` must be what actually landed, and {p} did not"
        );
    }
    assert!(paths[0].as_str().unwrap().ends_with(".md"));
    assert!(paths[1].as_str().unwrap().ends_with(".logdata.jsonl"));
    assert!(paths[2].as_str().unwrap().ends_with(".spandata.jsonl"));

    // The stem is the domain name by default — the only identity logmon has.
    assert!(r["stem"].as_str().unwrap().starts_with("default-"), "{r}");

    // Counts on the wire are the counts on disk, not a second computation.
    let logdata = std::fs::read_to_string(paths[1].as_str().unwrap()).unwrap();
    assert_eq!(
        r["logdata"]["records"].as_u64().unwrap(),
        logdata.lines().count() as u64 - 1,
        "the header is not a record: {r}"
    );
    assert_eq!(
        r["logdata"]["bytes"].as_u64().unwrap(),
        logdata.len() as u64
    );
    assert_eq!(
        logdata.lines().next().unwrap(),
        r#"{"logmon_format":1,"kind":"logdata"}"#
    );
    // A file with no spans is still written, with its header.
    let spandata = std::fs::read_to_string(paths[2].as_str().unwrap()).unwrap();
    assert_eq!(r["spandata"]["records"], 0);
    assert_eq!(
        spandata.lines().next().unwrap(),
        r#"{"logmon_format":1,"kind":"spandata"}"#
    );

    let doc = document_of(&r);
    assert_eq!(
        r["document_bytes"].as_u64().unwrap(),
        doc.len() as u64,
        "the measured artifact is the one that can run away"
    );
    assert!(doc.contains("# the interesting one"), "{doc}");
}

/// §5.1: rejected, not resolved. The broker runs as a service, so a relative
/// path would resolve against ITS working directory — and silently writing the
/// archive somewhere nobody looks is the failure mode.
#[test]
fn a_relative_dir_is_rejected_rather_than_resolved() {
    let h = harness();
    let anchor = h.feed(Level::Error, "boom");

    let err = h
        .call(
            "cases.create",
            json!({ "reason": "r", "dir": "docs/cases", "anchor": { "seq": anchor } }),
        )
        .expect_err("a relative dir must not be resolved against the daemon's cwd");
    assert!(err.contains("absolute"), "{err}");
    assert!(
        err.contains("working directory"),
        "the reason has to be in the message, or the caller just re-sends it: {err}"
    );
    assert!(
        !std::path::Path::new("docs/cases").exists(),
        "and nothing was written"
    );
}

/// §5.1: an unresolvable anchor is an error, not a degraded document. The
/// anchor's message is the headline, and a document whose headline cannot
/// identify the incident fails the one test §5.2 sets for a headline.
#[test]
fn an_unresolvable_anchor_is_an_error_not_a_headless_document() {
    let h = harness();
    h.feed(Level::Info, "something");

    let err = h
        .capture(json!({ "anchor": { "seq": 99_999 } }))
        .expect_err("no entry has that seq");
    assert!(err.contains("99999"), "{err}");
    assert!(err.contains("anchor"), "{err}");

    let err = h
        .capture(json!({ "anchor": { "bookmark": "nope" } }))
        .expect_err("no such bookmark");
    assert!(err.contains("nope"), "{err}");

    let err = h
        .capture(json!({ "anchor": { "trace_id": "deadbeef" } }))
        .expect_err("no entry carries that trace");
    assert!(err.contains("deadbeef"), "{err}");

    assert_eq!(
        std::fs::read_dir(h.archive.path()).unwrap().count(),
        0,
        "a failed capture leaves no half-written case behind"
    );
}

/// Tagged, not sniffed — a bookmark named `12345` and a seq are
/// indistinguishable as strings, and the failure would be silent.
#[test]
fn an_anchor_takes_exactly_one_of_three_and_the_error_names_them() {
    let h = harness();
    let anchor = h.feed(Level::Info, "x");

    for bad in [json!({}), json!({ "seq": anchor, "trace_id": "ab" })] {
        let err = h
            .capture(json!({ "anchor": bad }))
            .expect_err("exactly one");
        assert!(err.contains("seq"), "{err}");
        assert!(err.contains("bookmark"), "{err}");
        assert!(err.contains("trace_id"), "{err}");
    }

    let err = h
        .call("cases.create", json!({ "reason": "r", "dir": h.dir() }))
        .expect_err("anchor is required");
    assert!(err.contains("anchor"), "{err}");
}

/// §5.1: a `trace_id` matching many entries anchors on the earliest by seq
/// **and says so** — picking silently would make a wrong anchor a wrong
/// document rather than a wrong parameter.
#[test]
fn a_trace_id_matching_many_anchors_on_the_earliest_and_says_so() {
    let h = harness();
    h.feed(Level::Info, "unrelated");
    let first = h.feed_traced(Level::Warn, "first of the trace", Some(0x7f3a));
    h.feed_traced(Level::Error, "second of the trace", Some(0x7f3a));
    h.feed_traced(Level::Info, "third of the trace", Some(0x7f3a));

    let r = h
        .capture(json!({ "anchor": { "trace_id": "7f3a" } }))
        .unwrap();
    let doc = document_of(&r);

    assert!(doc.contains("# first of the trace"), "{doc}");
    assert!(doc.contains("matched 3 entries"), "{doc}");
    assert!(doc.contains(&format!("earliest by seq ({first})")), "{doc}");
    assert!(doc.contains("anchor: {kind: trace_id"), "{doc}");
}

/// A bookmark marks a BOUNDARY, not a record: `b>=name` selects strictly after
/// it. The anchor is therefore the first stored entry the same `b>=name` would
/// hand back — not an off-by-one only this call has.
#[test]
fn a_bookmark_anchor_takes_the_first_stored_entry_after_the_mark() {
    let h = harness();
    h.feed(Level::Info, "before the mark");
    h.call("bookmarks.add", json!({ "name": "mark" })).unwrap();
    let next = h.feed(Level::Info, "after the mark");

    let r = h
        .capture(json!({ "anchor": { "bookmark": "mark" } }))
        .unwrap();
    let doc = document_of(&r);
    assert!(doc.contains("# after the mark"), "{doc}");
    assert!(doc.contains(&format!("seq: {next}")), "{doc}");
}

/// The document's most important correctness property must not be reachable
/// only by parsing markdown — and two places computing it is how they come to
/// disagree.
#[test]
fn the_verdict_on_the_wire_equals_the_verdict_in_the_document() {
    let h = harness();
    let noisy = h.sessions.create_named("noisy").unwrap();

    h.feed(Level::Info, "unfiltered");
    let clean = h.capture(json!({ "anchor": { "seq": 1 } })).unwrap();
    assert_eq!(clean["verdict"], "complete", "{clean}");
    assert!(document_of(&clean).contains("verdict: complete"));

    // Another session narrows the domain, and the capture must say so.
    h.sessions
        .add_filter(&noisy, "l>=ERROR", Some("errors only"))
        .unwrap();
    h.feed(Level::Info, "never stored");
    let anchor = h.feed(Level::Error, "boom");

    let narrowed = h.capture(json!({ "anchor": { "seq": anchor } })).unwrap();
    assert_eq!(narrowed["verdict"], "filtered", "{narrowed}");
    let doc = document_of(&narrowed);
    assert!(doc.contains("verdict: filtered"), "{doc}");
    assert!(doc.contains("`l>=ERROR`"), "the filter is named: {doc}");

    // And a caller can act on it without parsing markdown.
    let notes = narrowed["notes"].as_array().expect("notes");
    assert!(
        notes.iter().any(|n| n["kind"] == "capture_gap"),
        "every note carries a kind: {narrowed}"
    );
    assert!(notes
        .iter()
        .all(|n| n["kind"].is_string() && n["detail"].is_string()));
}

/// §5.1: unsigilled `data` is applied to the registry **before** the copy is
/// rendered, so a key the capturer supplies appears in *this* document rather
/// than landing one document late.
#[test]
fn data_is_applied_before_the_registry_copy_is_rendered() {
    let h = harness();
    let anchor = h.feed(Level::Error, "boom");

    let r = h
        .capture(json!({
            "anchor": { "seq": anchor },
            "data": [
                { "path": "/Build/commit", "value": "9f2a1c4" },
                { "path": "/Env/host", "value": "ci-7" },
            ],
        }))
        .unwrap();

    let doc = document_of(&r);
    assert!(
        doc.contains("9f2a1c4"),
        "the value landed in THIS document: {doc}"
    );
    assert!(doc.contains("/Env/host"), "{doc}");
    // `/Env/host` is contextual, not core — coverage counts the three core keys
    // and NAMES the two still missing.
    assert!(doc.contains("1 of 3 core keys"), "{doc}");
    assert!(doc.contains("/Build/profile"), "{doc}");

    // Same outcomes as domain_data.update, because it is the same call.
    let outcomes = r["data_outcomes"].as_array().expect("per-entry outcomes");
    assert_eq!(outcomes.len(), 2, "{r}");
    assert!(outcomes.iter().all(|o| o["outcome"] == "created"), "{r}");

    // And the registry really holds them afterwards.
    let got = h.call("domain_data.get", json!({})).unwrap();
    let paths: Vec<&str> = got["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"/Build/commit"), "{got}");
}

/// §3.9: a sigilled key is about THIS capture. It reaches the document and the
/// registry is byte-identical afterwards — letting it in would mean the next
/// case on this domain silently inherits the last one's seed.
#[test]
fn a_sigilled_key_reaches_the_document_and_never_the_registry() {
    let h = harness();
    let anchor = h.feed(Level::Error, "boom");
    h.call(
        "domain_data.update",
        json!({ "entries": [{ "path": "/Action", "value": "checkout smoke" }] }),
    )
    .unwrap();
    let before = h.call("domain_data.get", json!({})).unwrap();

    let r = h
        .capture(json!({
            "anchor": { "seq": anchor },
            "data": [{ "path": "@/Data/seed", "value": "8814" }],
        }))
        .unwrap();

    let doc = document_of(&r);
    // The sigil SURVIVES into the rendering, so a reader grepping for
    // `/Data/seed` and one grepping for `@/Data/seed` ask two questions.
    assert!(
        doc.contains(r#"asserted: {"@/Data/seed": "8814"}"#),
        "{doc}"
    );
    assert!(doc.contains("- `@/Data/seed`: 8814"), "{doc}");
    assert!(
        doc.contains("not validated"),
        "an assertion is not a validated fact: {doc}"
    );

    let after = h.call("domain_data.get", json!({})).unwrap();
    assert_eq!(
        before, after,
        "the registry must be untouched by a sigilled key"
    );

    // Its own outcome: `created` would say the domain knows something it
    // deliberately does not, and `rejected` would say the assertion was lost.
    let outcomes = r["data_outcomes"].as_array().unwrap();
    assert_eq!(outcomes[0]["outcome"], "scoped", "{r}");
}

/// A sigil scopes a key; it does not launder the reservation.
#[test]
fn a_sigilled_reserved_key_is_rejected_and_the_capture_still_happens() {
    let h = harness();
    let anchor = h.feed(Level::Error, "boom");

    let r = h
        .capture(json!({
            "anchor": { "seq": anchor },
            "data": [
                { "path": "@/logmon/version", "value": "9.9.9" },
                { "path": "/Action", "value": "checkout smoke" },
            ],
        }))
        .unwrap();

    let outcomes = r["data_outcomes"].as_array().unwrap();
    assert_eq!(outcomes[0]["outcome"], "rejected", "{r}");
    assert_eq!(outcomes[0]["reason"], "reserved_prefix", "{r}");
    // One malformed entry does not reject the batch, and the capture happened.
    assert_eq!(outcomes[1]["outcome"], "created", "{r}");
    assert!(document_of(&r).contains("checkout smoke"));
    assert!(!document_of(&r).contains("9.9.9"));
}

/// The window is the caller's `before`/`after` in RECORDS, and the seq range it
/// resolves to is what scopes the spans — so the two files describe one
/// interval by construction.
#[test]
fn before_and_after_count_records_and_size_the_logdata() {
    let h = harness();
    for i in 1..=20 {
        h.feed(Level::Info, &format!("entry {i}"));
    }
    let anchor = h.feed(Level::Info, "anchor");
    for i in 1..=20 {
        h.feed(Level::Info, &format!("after {i}"));
    }

    let r = h
        .capture(json!({ "anchor": { "seq": anchor }, "before": 3, "after": 2 }))
        .unwrap();
    assert_eq!(
        r["logdata"]["records"], 6,
        "three before, the anchor, two after: {r}"
    );

    // The document shows the anchor and its neighbours, not the whole window.
    let doc = document_of(&r);
    assert!(doc.contains("> "), "the anchor is marked: {doc}");
    assert!(doc.contains("## 7. Evidence files"), "{doc}");
}

/// Two captures in the same second under one prefix produce two documents.
#[test]
fn a_second_capture_in_the_same_second_does_not_overwrite_the_first() {
    let h = harness();
    let anchor = h.feed(Level::Error, "boom");

    let a = h
        .capture(json!({ "anchor": { "seq": anchor }, "prefix": "x" }))
        .unwrap();
    let b = h
        .capture(json!({ "anchor": { "seq": anchor }, "prefix": "x" }))
        .unwrap();

    assert_ne!(a["stem"], b["stem"], "{a} vs {b}");
    assert_eq!(
        std::fs::read_dir(h.archive.path()).unwrap().count(),
        6,
        "three files each, none clobbered"
    );
}

/// The gate found `EvidenceVerdict::Evicted` structurally unreachable here: the
/// window's lower end comes from `context_by_seq`, which only returns STORED
/// entries, so it could never sit below the oldest one. A capture that asked for
/// 100 records of context and got 29 reported `complete`.
#[test]
fn a_window_the_ring_has_eaten_reports_evicted_not_complete() {
    let h = harness_with_log_capacity(30);
    for i in 1..=200 {
        h.feed(Level::Info, &format!("entry {i}"));
    }
    let anchor = h.feed(Level::Info, "anchor");

    let r = h
        .capture(json!({ "anchor": { "seq": anchor }, "before": 100, "after": 5 }))
        .unwrap();

    assert_eq!(
        r["verdict"], "evicted",
        "100 records of context were asked for and the ring had already dropped most of \
         them: {r}"
    );
    let doc = document_of(&r);
    assert!(doc.contains("**narrower than requested**"), "{doc}");
    assert!(doc.contains("**gone** rather than never recorded"), "{doc}");
    assert!(
        doc.contains("Raise the domain's `log_buffer_size`"),
        "{doc}"
    );
    assert!(
        r["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["kind"] == "capture_gap"),
        "{r}"
    );
}

/// `evicted` reports the store's floor and the caller's shortfall as two
/// separate facts, and **claims no count of what was lost**.
///
/// The defect: `short_before` — bounded by `before`, not by what the ring
/// dropped — was handed to the document as records-lost. The test above passed
/// only by luck of its fixture: 201 fed into a ring of 30 really did lose 171,
/// so an over-claim of 71 stayed under the true figure. Thirty-two records is
/// the input that separates them, and it is the ordinary shape of a young
/// domain rather than a contrived one.
#[test]
fn an_evicted_capture_claims_no_count_of_what_the_ring_dropped() {
    let h = harness_with_log_capacity(30);
    for i in 1..=31 {
        h.feed(Level::Info, &format!("entry {i}"));
    }
    let anchor = h.feed(Level::Info, "anchor");

    let r = h
        .capture(json!({ "anchor": { "seq": anchor }, "before": 100, "after": 0 }))
        .unwrap();
    assert_eq!(r["verdict"], "evicted", "{r}");

    let doc = document_of(&r);
    // Two records ever left this ring. The window is short by 71.
    assert!(
        doc.contains("It starts at seq 3, the ring has dropped everything below that"),
        "the store fact is a seq — the floor it can still speak for: {doc}"
    );
    assert!(
        doc.contains("71 record(s) short before the anchor"),
        "the request fact is a count of what did not come back: {doc}"
    );
    assert!(
        doc.contains("bounded by what was REQUESTED, not by what was lost"),
        "and the verdict says why that 71 is not a count of lost records: {doc}"
    );
    assert!(
        doc.contains("not knowable from here"),
        "nor can the split between them be recovered: {doc}"
    );
    for overclaim in ["71 records are gone", "up to 71", "at least 71"] {
        assert!(
            !doc.contains(overclaim),
            "this domain never held 71 records, so it cannot have lost them \
             ({overclaim}):\n{doc}"
        );
    }
    let note = r["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["kind"] == "capture_gap")
        .expect("evicted raises a capture-gap note")
        .to_string();
    assert!(!note.contains("up to 71"), "and the note agrees: {note}");
}

/// Both ends of the shortfall reach the front matter, where a tool reads them.
///
/// Nothing read `requested_after_missing` at all, and nothing pinned the
/// magnitude of either: hard-wiring the anchor's position to 0 — which makes
/// every document claim to be the whole of `before` short — survived the suite.
/// The two numbers here are deliberately different from each other and from
/// zero, so a swap or a constant is caught.
#[test]
fn the_front_matter_carries_the_shortfall_at_both_ends() {
    let h = harness();
    // Twenty records, anchored on the tenth: nine below it, ten above.
    let mut seqs = Vec::new();
    for i in 1..=20 {
        seqs.push(h.feed(Level::Info, &format!("entry {i}")));
    }
    let anchor = seqs[9];

    let r = h
        .capture(json!({ "anchor": { "seq": anchor }, "before": 100, "after": 100 }))
        .unwrap();
    let doc = document_of(&r);

    assert!(
        doc.contains("requested_before_missing: 91"),
        "100 asked for, 9 stored below the anchor: {doc}"
    );
    assert!(
        doc.contains("requested_after_missing: 90"),
        "100 asked for, 10 stored above it: {doc}"
    );
    assert!(
        doc.contains("91 record(s) short before the anchor and 90 after"),
        "and the body states both, in the caller's terms: {doc}"
    );

    // Nothing ever left this ring, so neither end is a loss — and the two ends
    // get different explanations.
    assert_eq!(r["verdict"], "complete", "{r}");
    assert!(doc.contains("empty past rather than a loss"), "{doc}");
    assert!(doc.contains("history that had not happened yet"), "{doc}");
}

/// A failure DOWNSTREAM of the claim gives the whole claim back.
///
/// `a_failed_capture_does_not_commit_data_to_the_registry` cannot see this: its
/// unresolvable anchor fails long before a stem is taken, so the rollback path
/// never runs and deleting it entirely leaves that test green — a vacuous
/// scenario rather than a vacuous assertion. A malformed `data` entry fails
/// after the files exist, which is the only input that exercises the rollback.
#[test]
fn a_capture_that_fails_after_claiming_gives_the_stem_back() {
    let h = harness();
    let anchor = h.feed(Level::Error, "boom");

    let err = h
        .capture(json!({
            "anchor": { "seq": anchor },
            "data": [{ "value": "no path here" }],
        }))
        .expect_err("a data entry without a path is malformed");
    assert!(err.contains("path"), "and the error names it: {err}");

    assert_eq!(
        std::fs::read_dir(h.archive.path()).unwrap().count(),
        0,
        "the three claimed files are gone — a failed capture leaves no empty \
         document behind"
    );

    // And the stem is genuinely free: a retry takes it rather than rolling to a
    // second id, which is what a claim that was released means.
    let ok = h.capture(json!({ "anchor": { "seq": anchor } })).unwrap();
    let stem = std::path::Path::new(ok["paths"][0].as_str().unwrap())
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        !stem.contains("-2."),
        "a stranded claim would push the retry onto a rolled id: {stem}"
    );
    assert_eq!(std::fs::read_dir(h.archive.path()).unwrap().count(), 3);
}

/// The mirror, and the reason `lost_below` exists rather than `oldest_seq`: a
/// young domain has not lost anything, however far its first record sits above
/// seq 1. One counter feeds both stores, so the seqs below a span ring's oldest
/// entry belonged to logs — reporting them as evicted spans is a claim about
/// records that never existed.
#[test]
fn a_young_domain_is_not_reported_as_having_lost_anything() {
    let h = harness();
    // Spans first, so the span ring's oldest seq is well above 1 with nothing
    // ever evicted.
    let d = h.domains.get(&DomainId::default_domain()).unwrap();
    for i in 1..=5 {
        h.feed(Level::Info, &format!("log {i}"));
    }
    let anchor = h.feed(Level::Error, "boom");
    d.span_store.insert(a_span());

    let r = h.capture(json!({ "anchor": { "seq": anchor } })).unwrap();
    assert_eq!(r["verdict"], "complete", "{r}");

    let doc = document_of(&r);
    assert!(
        !doc.contains("are gone"),
        "nothing was ever dropped, so nothing may be reported gone:\n{doc}"
    );
    assert!(doc.contains("dropped nothing below seq"), "{doc}");
}

/// Bookmarks are session-scoped, and `b>=name` resolves them that way. A bare
/// name that scanned across sessions would silently anchor the document on
/// another session's stale mark — and since the anchor's message becomes the
/// headline, that is a wrong document rather than a wrong parameter.
#[test]
fn a_bookmark_anchor_does_not_reach_another_sessions_mark() {
    let h = harness();
    h.feed(Level::Info, "first");
    // Another session's `boom`, at a much later position.
    let other = h.sessions.create_named("cli").unwrap();
    let req = RpcRequest::new(1, "bookmarks.add", json!({ "name": "boom" }));
    h.handler.handle(&other, &req);
    h.feed(Level::Info, "second");

    let err = h
        .capture(json!({ "anchor": { "bookmark": "boom" } }))
        .expect_err("this session has no bookmark by that name");
    assert!(err.contains("capturer/boom"), "{err}");
    assert!(
        err.contains("session-scoped"),
        "and it says how to reach another session's: {err}"
    );

    // Explicitly qualified, it resolves — to the first entry after `cli`'s
    // mark, which was placed when only "first" had arrived.
    let r = h
        .capture(json!({ "anchor": { "bookmark": "cli/boom" } }))
        .unwrap();
    assert!(document_of(&r).contains("# second"), "{r}");
}

/// An empty optional array is not an error, and losing the evidence over one
/// would be absurd.
#[test]
fn an_empty_data_list_does_not_abort_the_capture() {
    let h = harness();
    let anchor = h.feed(Level::Error, "boom");
    let r = h
        .capture(json!({ "anchor": { "seq": anchor }, "data": [] }))
        .unwrap();
    assert_eq!(r["data_outcomes"].as_array().unwrap().len(), 0, "{r}");
    assert!(std::path::Path::new(r["paths"][0].as_str().unwrap()).exists());
}

/// `data` is a durable side effect on a shared store. A capture that fails must
/// not leave it applied with no document written and the caller never told.
#[test]
fn a_failed_capture_does_not_commit_data_to_the_registry() {
    let h = harness();
    h.feed(Level::Info, "something");

    let err = h
        .capture(json!({
            "anchor": { "seq": 99_999 },
            "data": [{ "path": "/Action", "value": "reproducing the hang" }],
        }))
        .expect_err("the anchor does not resolve");
    assert!(err.contains("99999"), "{err}");

    let got = h.call("domain_data.get", json!({})).unwrap();
    let paths: Vec<&str> = got["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["path"].as_str().unwrap())
        .collect();
    assert!(
        !paths.contains(&"/Action"),
        "the registry must be untouched by a capture that never happened: {got}"
    );
    assert_eq!(
        std::fs::read_dir(h.archive.path()).unwrap().count(),
        0,
        "and no stem is burned"
    );
}

/// §5.4's selection, which had no test at all: returning an empty list, or
/// dropping the domain filter so collectors from *every* domain appear, both
/// passed the whole suite.
#[test]
fn collector_state_is_selected_by_domain_across_owners() {
    let h = harness();
    h.domains.insert(make_domain("other"));
    let anchor = h.feed(Level::Error, "boom");

    // Armed by a DIFFERENT session than the one capturing — the CLI and the
    // shim are different sessions on one domain, which is why the selection is
    // across owners.
    let cli = h.sessions.create_named("cli").unwrap();
    let req = RpcRequest::new(
        1,
        "collectors.add",
        json!({ "name": "checkout", "filter": "sv=svc" }),
    );
    assert!(h.handler.handle(&cli, &req).error.is_none());

    // And one pinned to another domain, which must NOT appear.
    let elsewhere = h.sessions.create_named("elsewhere").unwrap();
    h.handler.handle(
        &elsewhere,
        &RpcRequest::new(1, "domains.use", json!({ "name": "other" })),
    );
    let req = RpcRequest::new(
        1,
        "collectors.add",
        json!({ "name": "faraway", "filter": "sv=svc" }),
    );
    assert!(h.handler.handle(&elsewhere, &req).error.is_none());

    let doc = document_of(&h.capture(json!({ "anchor": { "seq": anchor } })).unwrap());
    assert!(doc.contains("| `checkout` | `cli` |"), "{doc}");
    assert!(
        !doc.contains("faraway"),
        "a collector pinned to another domain is not this domain's state: {doc}"
    );
    assert!(
        !doc.contains("No collectors were armed"),
        "the empty branch must not be the one every test takes: {doc}"
    );
}

/// A recorded run's label and age reach the document — `snapshot_count` alone
/// cannot say whether the measurement predates the build being investigated.
#[test]
fn a_collectors_latest_run_reaches_the_document() {
    let h = harness();
    let anchor = h.feed(Level::Error, "boom");
    h.call(
        "collectors.add",
        json!({ "name": "checkout", "filter": "sv=svc" }),
    )
    .unwrap();
    h.call(
        "collectors.snapshot",
        json!({ "name": "checkout", "label": "before" }),
    )
    .unwrap();

    let doc = document_of(&h.capture(json!({ "anchor": { "seq": anchor } })).unwrap());
    assert!(doc.contains("`before`,"), "the run's label: {doc}");
    assert!(doc.contains("ago |"), "and its age: {doc}");
}

/// Every earlier test asserted `spandata.records == 0`, so deleting the span
/// gather entirely passed the suite — while the test comment claimed the two
/// files "describe one interval by construction".
///
/// **The expectation here FLIPPED from 2 to 3 on 2026-08-04, and that is the
/// point.** This test used to assert that a span arriving after the last log was
/// excluded, and its comment called that span "the proof, by being excluded".
/// It was proof of a defect: the window was derived from logs alone and then
/// applied to spans, and OTLP exports a span when it ENDS, so the spans of a
/// slow operation land precisely there. `before`/`after` now count records in
/// the shared seq space, so the span above the anchor is one of the `after`
/// records and belongs in the window. The "one interval" property the old
/// comment defended is preserved — the interval is just cut from both stores
/// now, which is what makes it one.
#[test]
fn spans_over_the_resolved_range_land_in_the_spandata_file() {
    let h = harness();
    let d = h.domains.get(&DomainId::default_domain()).unwrap();
    h.feed(Level::Info, "before");
    d.span_store.insert(a_span());
    d.span_store.insert(a_span());
    let anchor = h.feed(Level::Error, "boom");
    // One record above the anchor in the merged space, so it is inside `after`.
    d.span_store.insert(a_span());

    let r = h.capture(json!({ "anchor": { "seq": anchor } })).unwrap();
    assert_eq!(
        r["spandata"]["records"], 3,
        "two spans below the anchor and the one above it, all within `after`: {r}"
    );

    let text = std::fs::read_to_string(r["paths"][2].as_str().unwrap()).unwrap();
    assert_eq!(
        text.lines().count() as u64 - 1,
        3,
        "and they are on disk, not merely counted"
    );
    assert!(text.contains(r#""service_name":"svc""#), "{text}");
    assert!(document_of(&r).contains("Slowest spans"), "and summarised");
}

/// The defect this fix exists for, reproduced from the live probe that found it.
///
/// Measured 2026-08-03 against a real broker: six GELF logs took seqs 1001–1006,
/// three OTLP spans took 1007–1009, and a case anchored on the error log
/// captured **zero** spans — none of the trace it was about. The spans had been
/// stored 95 seconds *before* the capture ran, so nothing was racing; the window
/// was simply derived from one store and applied to two.
///
/// The document then said "seq 1006 was the newest record stored when the
/// capture was taken, so the shortfall after the anchor is history that had not
/// happened yet ... nothing was lost here" — false in both clauses.
///
/// **Under the log-derived window this test reports 0 spans and asserts against
/// it.** That is its red-first criterion.
#[test]
fn spans_arriving_after_the_last_log_are_captured_not_silently_dropped() {
    let h = harness();
    let d = h.domains.get(&DomainId::default_domain()).unwrap();

    // The shape a real service produces: the logs of an operation, then the
    // spans of that same operation, because a span is exported when it ends.
    h.feed(Level::Info, "checkout started");
    h.feed(Level::Warn, "payment gateway slow");
    let anchor = h.feed(Level::Error, "payment gateway timeout");
    d.span_store.insert(a_span());
    d.span_store.insert(a_span());
    d.span_store.insert(a_span());

    let r = h.capture(json!({ "anchor": { "seq": anchor } })).unwrap();

    assert_eq!(
        r["spandata"]["records"], 3,
        "the spans of the failing operation must be in the case: {r}"
    );

    // And the document's claim about the top of the window must be TRUE.
    //
    // The sentence at `document.rs:531` — "seq {to} was the newest record stored
    // when the capture was taken" — still fires whenever `after` was not filled,
    // and that is correct: what makes it a lie is `to` not being the newest
    // record. So assert the property, not the absence of the sentence. (An
    // earlier draft of this test asserted the sentence never appears, which was
    // the same mistake in the other direction.)
    let fm = logmon_broker_core::cases::parse_front_matter(&document_of(&r)).unwrap();
    assert_eq!(
        fm.seq_range.to,
        d.span_store.newest_seq().unwrap(),
        "the window's upper end must be the newest record in the domain, across \
         BOTH stores — that is what makes document.rs:531's sentence true"
    );
    assert_eq!(
        fm.spandata.records, 3,
        "and the front matter agrees with the file"
    );
}

/// The span ring's own retention reaches the document, and the note.
#[test]
fn a_span_ring_that_dropped_spans_says_so_in_the_document() {
    let h = harness();
    h.domains.insert(make_domain_spans("default", 4));
    let d = h.domains.get(&DomainId::default_domain()).unwrap();
    h.feed(Level::Info, "first");
    for _ in 0..10 {
        d.span_store.insert(a_span());
    }
    let anchor = h.feed(Level::Error, "boom");

    let r = h
        .capture(json!({ "anchor": { "seq": anchor }, "before": 50 }))
        .unwrap();
    let doc = document_of(&r);
    assert!(
        doc.contains("**had** evicted below seq"),
        "the span ring's own loss, separate from the log verdict: {doc}"
    );
    assert!(
        doc.contains("The log verdict above does not cover this"),
        "{doc}"
    );
    assert!(
        r["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["detail"].as_str().unwrap().contains("span ring")),
        "{r}"
    );
}

/// The document shows the anchor and ten neighbours; the logdata holds the rest.
///
/// The earlier version of this used a 6-record window against a 10-record
/// neighbourhood, so "show ten either side" and "show everything" were the same
/// hypothesis on that input — both `NEIGHBOUR_WINDOW = 0` and "return the whole
/// window" passed it.
#[test]
fn the_document_shows_the_neighbourhood_and_the_logdata_holds_the_window() {
    let h = harness();
    for i in 1..=80 {
        h.feed(Level::Info, &format!("entry {i}"));
    }
    let anchor = h.feed(Level::Info, "anchor");
    for i in 1..=80 {
        h.feed(Level::Info, &format!("after {i}"));
    }

    let r = h
        .capture(json!({ "anchor": { "seq": anchor }, "before": 60, "after": 60 }))
        .unwrap();
    assert_eq!(r["logdata"]["records"], 121, "the full window: {r}");

    let doc = document_of(&r);
    let fenced = doc
        .split("```")
        .nth(1)
        .expect("the neighbours block is fenced");
    assert_eq!(
        fenced
            .lines()
            .filter(|l| l.contains("  entry ") || l.contains("  after ") || l.contains("  anchor"))
            .count(),
        21,
        "ten either side of the anchor, not the whole window and not only the anchor:\n{fenced}"
    );
}

/// The 5000 ceiling is the only thing between a typo and a multi-megabyte RPC,
/// and it had no test — removing the clamp entirely passed the suite.
#[test]
fn an_over_wide_request_is_clamped_and_said_to_be() {
    let h = harness();
    for i in 1..=20 {
        h.feed(Level::Info, &format!("entry {i}"));
    }
    let anchor = h.feed(Level::Info, "anchor");

    let r = h
        .capture(json!({ "anchor": { "seq": anchor }, "before": 999_999 }))
        .unwrap();
    let doc = document_of(&r);
    assert!(doc.contains("clamped: true"), "{doc}");
    assert!(doc.contains("clamped to the maximum"), "{doc}");
    assert!(
        r["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["kind"] == "truncated"),
        "{r}"
    );
    // And it must not contradict the verdict — the defect this replaces put
    // "nothing was capped" and "the read was capped" in one document. Asserted
    // on the word rather than on the old sentences, which exist nowhere now and
    // so could never have fired again.
    assert_eq!(r["verdict"], "complete", "{r}");
    assert!(
        !doc.contains("capped"),
        "a clamped window is not a capped one, in any wording: {doc}"
    );
}

/// `/logmon/*` is the daemon's own bookkeeping and stays out of the rendered
/// registry copy — `incarnation` is already in front matter, and listing
/// `/logmon/first_seen` beside `/Build/commit` presents the two as one kind of
/// claim.
#[test]
fn the_reserved_namespace_stays_out_of_the_registry_copy() {
    let h = harness();
    let anchor = h.feed(Level::Error, "boom");
    h.call(
        "domain_data.update",
        json!({ "entries": [{ "path": "/Build/commit", "value": "9f2a1c4" }] }),
    )
    .unwrap();

    let r = h.capture(json!({ "anchor": { "seq": anchor } })).unwrap();
    let doc = document_of(&r);
    let provenance = doc.split("## 5. Provenance").nth(1).unwrap();
    assert!(provenance.contains("/Build/commit"), "{provenance}");
    assert!(
        !provenance.contains("/logmon/"),
        "the daemon's own bookkeeping is not the capturer's provenance: {provenance}"
    );
    // But the incarnation IS carried, once, where a tool can read it.
    assert!(doc.contains("incarnation: \"1\""), "{doc}");
}

/// §5.3: the prefix falls back parameter → `/case-name` → domain name.
#[test]
fn the_prefix_falls_back_through_the_registry_to_the_domain() {
    let h = harness();
    let anchor = h.feed(Level::Error, "boom");

    let by_domain = h.capture(json!({ "anchor": { "seq": anchor } })).unwrap();
    assert!(by_domain["stem"].as_str().unwrap().starts_with("default-"));

    h.call(
        "domain_data.update",
        json!({ "entries": [{ "path": "/case-name", "value": "ht-server" }] }),
    )
    .unwrap();
    let by_registry = h.capture(json!({ "anchor": { "seq": anchor } })).unwrap();
    assert!(by_registry["stem"]
        .as_str()
        .unwrap()
        .starts_with("ht-server-"));

    let by_param = h
        .capture(json!({ "anchor": { "seq": anchor }, "prefix": "checkout-hang" }))
        .unwrap();
    assert!(by_param["stem"]
        .as_str()
        .unwrap()
        .starts_with("checkout-hang-"));
}
