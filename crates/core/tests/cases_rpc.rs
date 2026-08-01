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
    let config = tempfile::tempdir().expect("tempdir");
    let archive = tempfile::tempdir().expect("tempdir");
    let domains = Arc::new(DomainRegistry::new());
    domains.insert(make_domain("default"));
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
