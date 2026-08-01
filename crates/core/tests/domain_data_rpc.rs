//! `domain_data.*` over the RPC boundary — case-documents spec §3.
//!
//! Every harness here is handed a **scratch** config dir. A registry that
//! persisted would write into whatever directory it was given, and the live
//! daemon's is one wrong argument away.

use logmon_broker_core::daemon::domain::{
    Domain, DomainConfig, DomainId, DomainRegistry, DomainSource,
};
use logmon_broker_core::daemon::rpc_handler::{DomainPolicy, RpcHandler};
use logmon_broker_core::daemon::session::{SessionId, SessionRegistry};
use logmon_broker_core::domain_data::DomainDataStore;
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use logmon_broker_core::receiver::ReceiverMetrics;
use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::store::bookmarks::BookmarkStore;
use logmon_broker_protocol::{RpcRequest, RpcResponse};
use serde_json::{json, Value};
use std::sync::Arc;

struct Harness {
    handler: Arc<RpcHandler>,
    session: SessionId,
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
    domains.insert(make_domain("default"));
    let sessions = Arc::new(SessionRegistry::new());
    let session = SessionId::Named("t".into());
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
            dir.path().to_path_buf(),
            "0.9.0".into(),
        ))),
    );
    Harness {
        handler,
        session,
        _dir: dir,
    }
}

impl Harness {
    fn call(&self, method: &str, params: Value) -> RpcResponse {
        self.handler
            .handle(&self.session, &RpcRequest::new(1, method, params))
    }

    fn ok(&self, method: &str, params: Value) -> Value {
        let r = self.call(method, params);
        r.result
            .unwrap_or_else(|| panic!("{method} failed: {:?}", r.error))
    }

    fn update(&self, entries: Value) -> Value {
        self.ok("domain_data.update", json!({ "entries": entries }))
    }

    fn outcomes(&self, entries: Value) -> Vec<String> {
        self.update(entries)["outcomes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["outcome"].as_str().unwrap().to_string())
            .collect()
    }
}

#[test]
fn a_key_round_trips_through_update_and_get() {
    let h = harness();
    assert_eq!(
        h.outcomes(json!([{"path": "/Build/commit", "value": "9f2a1c4"}])),
        vec!["created"]
    );
    let got = h.ok("domain_data.get", json!({}));
    let key = got["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["path"] == "/Build/commit")
        .expect("present");
    assert_eq!(key["value"], "9f2a1c4");
    assert!(key["created_at"].is_string());
    assert!(key["validated_at"].is_string());
}

#[test]
fn logmons_own_keys_are_visible_but_not_writable() {
    let h = harness();
    let got = h.ok("domain_data.get", json!({"prefix": "/logmon"}));
    let paths: Vec<&str> = got["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"/logmon/domain"));
    assert!(paths.contains(&"/logmon/version"));

    let out = h.update(json!([{"path": "/logmon/version", "value": "99"}]));
    assert_eq!(out["outcomes"][0]["outcome"], "rejected");
    assert_eq!(out["outcomes"][0]["reason"], "reserved_prefix");
}

#[test]
fn the_bare_reserved_segment_is_refused_on_both_surfaces() {
    // A `starts_with("/logmon/")` guard accepts `/logmon`, and then it can
    // never be removed. Read the other way, remove("/logmon") would wipe
    // first_seen and incarnation — H3's and H6's whole defence.
    let h = harness();
    let out = h.update(json!([{"path": "/logmon", "value": "x"}]));
    assert_eq!(out["outcomes"][0]["reason"], "reserved_prefix");

    let out = h.ok("domain_data.remove", json!({"patterns": ["/logmon"]}));
    assert_eq!(out["outcomes"][0]["reason"], "reserved_prefix");
    assert_eq!(out["outcomes"][0]["removed"], 0);

    let got = h.ok("domain_data.get", json!({"prefix": "/logmon"}));
    assert!(!got["keys"].as_array().unwrap().is_empty(), "still there");
}

#[test]
fn restating_a_value_validates_and_a_new_value_updates() {
    let h = harness();
    assert_eq!(
        h.outcomes(json!([{"path": "/a", "value": "1"}])),
        vec!["created"]
    );
    assert_eq!(
        h.outcomes(json!([{"path": "/a", "value": "1"}])),
        vec!["validated"],
        "same value is a confirmation, not a change"
    );
    assert_eq!(
        h.outcomes(json!([{"path": "/a", "value": "2"}])),
        vec!["updated"]
    );
}

#[test]
fn a_key_only_entry_on_a_missing_key_reports_unknown_with_a_cause() {
    let h = harness();
    let out = h.update(json!([{"path": "/never"}]));
    assert_eq!(out["outcomes"][0]["outcome"], "unknown");
    // The registry exists — boot stamped /logmon/* — so this is never_set
    // rather than the honest-shrug undetermined.
    assert_eq!(out["outcomes"][0]["cause"], "never_set");

    let got = h.ok("domain_data.get", json!({"prefix": "/never"}));
    assert!(
        got["keys"].as_array().unwrap().is_empty(),
        "must not invent a valueless key"
    );
}

#[test]
fn a_null_value_validates_and_never_erases() {
    // A client serialising a missing field as null must not mean the opposite
    // of what it sent. Erasure is remove.
    let h = harness();
    h.update(json!([{"path": "/a", "value": "1"}]));
    assert_eq!(
        h.outcomes(json!([{"path": "/a", "value": null}])),
        vec!["validated"]
    );
    let got = h.ok("domain_data.get", json!({"prefix": "/a"}));
    assert_eq!(got["keys"][0]["value"], "1");
}

#[test]
fn one_bad_entry_does_not_reject_the_batch_and_order_is_preserved() {
    let h = harness();
    let out = h.update(json!([
        {"path": "/good", "value": "1"},
        {"path": "bad", "value": "2"},
        {"path": "/also", "value": "3"},
    ]));
    let os = out["outcomes"].as_array().unwrap();
    assert_eq!(os[0]["path"], "/good");
    assert_eq!(os[0]["outcome"], "created");
    assert_eq!(os[1]["path"], "bad");
    assert_eq!(os[1]["reason"], "malformed_path");
    assert_eq!(os[2]["path"], "/also");
    assert_eq!(os[2]["outcome"], "created");
}

#[test]
fn a_malformed_ttl_rejects_one_entry_in_place_rather_than_the_batch() {
    // The rejection is decided before the registry sees it, so this asserts
    // the splice puts it back in the caller's order.
    let h = harness();
    let out = h.update(json!([
        {"path": "/a", "value": "1"},
        {"path": "/b", "value": "2", "ttl": "1h30m"},
        {"path": "/c", "value": "3"},
    ]));
    let os = out["outcomes"].as_array().unwrap();
    assert_eq!(os.len(), 3);
    assert_eq!(os[0]["path"], "/a");
    assert_eq!(os[1]["path"], "/b");
    assert_eq!(os[1]["reason"], "malformed_ttl");
    assert_eq!(os[2]["path"], "/c");
    assert_eq!(os[2]["outcome"], "created");
}

#[test]
fn a_non_string_value_is_rejected_rather_than_read_as_absent() {
    // Absent means *validate*, so reading `42` as absent would silently confirm
    // the old value and report success for a set that never happened — a wrong
    // answer wearing the shape of a right one.
    let h = harness();
    h.update(json!([{"path": "/a", "value": "1"}]));
    let out = h.update(json!([{"path": "/a", "value": 42}]));
    assert_eq!(out["outcomes"][0]["outcome"], "rejected");
    assert_eq!(out["outcomes"][0]["reason"], "malformed_value");

    let got = h.ok("domain_data.get", json!({"prefix": "/a"}));
    assert_eq!(got["keys"][0]["value"], "1", "and nothing changed");
}

#[test]
fn a_repeated_path_does_not_reorder_the_reply() {
    // Splicing rejections back by *path* looks equivalent to splicing by index
    // and is not: every outcome for a repeated key would take the position of
    // its first occurrence, reordering a reply the caller matches positionally.
    let h = harness();
    let out = h.update(json!([
        {"path": "/dup", "value": "1"},
        {"path": "/mid",  "value": "2", "ttl": "nonsense"},
        {"path": "/dup", "value": "3"},
    ]));
    let os = out["outcomes"].as_array().unwrap();
    assert_eq!(os.len(), 3);
    assert_eq!(os[0]["path"], "/dup");
    assert_eq!(os[0]["outcome"], "created");
    assert_eq!(os[1]["path"], "/mid", "the rejection keeps its own slot");
    assert_eq!(os[1]["reason"], "malformed_ttl");
    assert_eq!(os[2]["path"], "/dup");
    assert_eq!(os[2]["outcome"], "updated", "last writer in the batch wins");

    let got = h.ok("domain_data.get", json!({"prefix": "/dup"}));
    assert_eq!(got["keys"][0]["value"], "3");
}

#[test]
fn a_ttl_reports_a_verdict_and_its_absence_reports_none() {
    let h = harness();
    h.update(json!([
        {"path": "/timed", "value": "x", "ttl": "30m"},
        {"path": "/untimed", "value": "y"},
    ]));
    let got = h.ok("domain_data.get", json!({}));
    let keys = got["keys"].as_array().unwrap();
    let timed = keys.iter().find(|k| k["path"] == "/timed").unwrap();
    assert_eq!(timed["ttl"], "30m");
    assert_eq!(timed["expired"], false);

    let untimed = keys.iter().find(|k| k["path"] == "/untimed").unwrap();
    assert!(
        untimed.get("expired").is_none(),
        "absent means unknown, not fresh — a key with no ttl gets an age and \
         no verdict, ever"
    );
    assert!(untimed["age_secs"].is_i64());
}

#[test]
fn ttl_false_clears_a_lifetime_in_place() {
    let h = harness();
    h.update(json!([{"path": "/a", "value": "1", "ttl": "30m"}]));
    h.update(json!([{"path": "/a", "ttl": false}]));
    let got = h.ok("domain_data.get", json!({"prefix": "/a"}));
    assert!(got["keys"][0].get("ttl").is_none());
    assert!(got["keys"][0].get("expired").is_none());
}

#[test]
fn remove_matches_on_segment_boundaries_and_counts_per_pattern() {
    let h = harness();
    h.update(json!([
        {"path": "/Versions/a", "value": "1"},
        {"path": "/Versions/b", "value": "2"},
        {"path": "/VersionsOld/c", "value": "3"},
    ]));
    let out = h.ok(
        "domain_data.remove",
        json!({"patterns": ["/Versions", "/Nothing"]}),
    );
    assert_eq!(out["outcomes"][0]["removed"], 2);
    assert_eq!(out["outcomes"][1]["removed"], 0);

    let got = h.ok("domain_data.get", json!({"prefix": "/VersionsOld"}));
    assert_eq!(
        got["keys"].as_array().unwrap().len(),
        1,
        "byte-prefix matching would have let /Ver delete /Versions/*"
    );
}

#[test]
fn get_reports_the_unfiltered_total_so_a_narrow_read_is_not_an_empty_one() {
    let h = harness();
    h.update(json!([{"path": "/a", "value": "1"}, {"path": "/b", "value": "2"}]));
    let got = h.ok("domain_data.get", json!({"prefix": "/a"}));
    assert_eq!(got["keys"].as_array().unwrap().len(), 1);
    assert!(
        got["total"].as_u64().unwrap() > 1,
        "a filtered read must not look like an empty registry"
    );
}

#[test]
fn get_names_the_missing_core_keys_rather_than_counting_them() {
    let h = harness();
    let got = h.ok("domain_data.get", json!({}));
    let missing: Vec<&str> = got["missing_core"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(missing, vec!["/Build/commit", "/Build/profile", "/Action"]);

    h.update(json!([{"path": "/Build/commit", "value": "abc"}]));
    let got = h.ok("domain_data.get", json!({}));
    let missing: Vec<&str> = got["missing_core"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        missing,
        vec!["/Build/profile", "/Action"],
        "\"missing /Build/profile\" is actionable where \"2 of 3\" is not"
    );
}

#[test]
fn a_sigil_key_is_refused_on_the_registry_surface() {
    // There is no document here to receive it. Rejected with a reason naming
    // why, never silently dropped.
    let h = harness();
    let out = h.update(json!([{"path": "@/Data/seed", "value": "8814"}]));
    assert_eq!(out["outcomes"][0]["outcome"], "rejected");
    assert_eq!(out["outcomes"][0]["reason"], "sigil_not_allowed_here");
}

#[test]
fn the_registry_survives_a_new_handler_over_the_same_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = SessionId::Named("t".into());
    let build = || {
        let domains = Arc::new(DomainRegistry::new());
        domains.insert(make_domain("default"));
        let sessions = Arc::new(SessionRegistry::new());
        Arc::new(
            RpcHandler::new(
                domains,
                sessions,
                Arc::new(logmon_broker_core::collector::registry::CollectorRegistry::new()),
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
        )
    };
    let call = |h: &Arc<RpcHandler>, method: &str, params: Value| {
        h.handle(&session, &RpcRequest::new(1, method, params))
            .result
            .expect("ok")
    };

    let first = build();
    call(
        &first,
        "domain_data.update",
        json!({"entries": [{"path": "/Build/commit", "value": "abc"}]}),
    );
    drop(first);

    let second = build();
    let got = call(&second, "domain_data.get", json!({"prefix": "/Build"}));
    assert_eq!(got["keys"][0]["value"], "abc");
}

#[test]
fn a_broker_without_the_registry_says_so_rather_than_reporting_it_empty() {
    // An empty registry and an absent one are different facts, and the second
    // must not be able to read as the first.
    let domains = Arc::new(DomainRegistry::new());
    domains.insert(make_domain("default"));
    let sessions = Arc::new(SessionRegistry::new());
    let session = SessionId::Named("t".into());
    let handler = RpcHandler::new(
        domains,
        sessions,
        Arc::new(logmon_broker_core::collector::registry::CollectorRegistry::new()),
        vec!["test".into()],
        DomainPolicy {
            max_domains: 32,
            default_log_buffer_size: 1000,
            default_span_buffer_size: 1000,
            stale_after_secs: 60,
        },
    );
    let r = handler.handle(&session, &RpcRequest::new(1, "domain_data.get", json!({})));
    assert!(r.result.is_none());
    assert!(r.error.is_some());
}

// ---------------------------------------------------------------------------
// §3.8 — `collectors.add(data)` IS `domain_data.update` on that call's domain
// ---------------------------------------------------------------------------

/// The shorthand writes the same registry, and says so in the same vocabulary.
///
/// It exists because recording what the project was, at the moment someone is
/// already describing what they are measuring, costs one parameter — where a
/// separate call costs a decision, and the decision is what does not get made.
#[test]
fn arming_a_collector_can_record_provenance_in_the_same_call() {
    let h = harness();
    let armed = h.ok(
        "collectors.add",
        json!({
            "name": "cache",
            "filter": "ALL",
            "data": [
                {"path": "/Build/commit", "value": "9f2a1c4"},
                {"path": "/Build/profile", "value": "release"},
            ],
        }),
    );

    let outcomes: Vec<&str> = armed["data_outcomes"]
        .as_array()
        .expect("one outcome per entry, in the order sent")
        .iter()
        .map(|o| o["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, vec!["created", "created"]);

    // The point of the shorthand: it is the registry, not a private copy.
    let got = h.ok("domain_data.get", json!({}));
    assert!(
        registry_paths(&got).contains(&"/Build/commit".to_string()),
        "the shorthand must write the domain registry, not a second store: {got}"
    );
}

/// Every path currently in the registry, for the assertions below.
fn registry_paths(got: &Value) -> Vec<String> {
    got["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .map(|k| k["path"].as_str().unwrap().to_string())
        .collect()
}

/// A reply says nothing about `data` when none was sent.
///
/// The schema declares both fields skippable; emitting an empty list and a null
/// on every `collectors.add` would teach a reader that the parameter did
/// something when it did nothing.
#[test]
fn arming_without_data_says_nothing_about_data() {
    let h = harness();
    let armed = h.ok(
        "collectors.add",
        json!({ "name": "plain", "filter": "ALL" }),
    );
    assert!(armed.get("data_outcomes").is_none(), "{armed}");
    assert!(armed.get("data_persist_error").is_none(), "{armed}");
}

/// A sigilled key is refused here, and refused by the shared implementation
/// rather than by a check written twice.
///
/// The sigil scopes a fact to one document. Arming a collector writes no
/// document, so there is nowhere for it to go — and flattening it into the
/// registry would launder exactly the distinction the sigil was added for.
#[test]
fn arming_refuses_a_key_scoped_to_a_document_that_does_not_exist() {
    let h = harness();
    let armed = h.ok(
        "collectors.add",
        json!({
            "name": "scoped",
            "filter": "ALL",
            "data": [{"path": "@/Data/seed", "value": "1234"}],
        }),
    );
    let outcome = &armed["data_outcomes"][0];
    assert_eq!(outcome["outcome"], "rejected", "{armed}");
    assert_eq!(outcome["reason"], "sigil_not_allowed_here", "{armed}");

    // Refused, not quietly written under a stripped key.
    let got = h.ok("domain_data.get", json!({}));
    assert!(
        !registry_paths(&got)
            .iter()
            .any(|p| p.contains("/Data/seed")),
        "a refused key must not reach the registry by either spelling: {got}"
    );
}

/// The reservation is not launderable through the shorthand either.
#[test]
fn arming_cannot_write_the_reserved_prefix() {
    let h = harness();
    let armed = h.ok(
        "collectors.add",
        json!({
            "name": "reserved",
            "filter": "ALL",
            "data": [{"path": "/logmon/version", "value": "9.9.9"}],
        }),
    );
    assert_eq!(armed["data_outcomes"][0]["outcome"], "rejected", "{armed}");
    assert_eq!(
        armed["data_outcomes"][0]["reason"], "reserved_prefix",
        "{armed}"
    );
}

/// Provenance is not left behind by a call that failed.
///
/// A caller who saw an error never saw an outcome list, so anything written
/// under it would be invisible state. The entries are applied only once the
/// collector is actually armed.
#[test]
fn a_refused_arming_records_no_provenance() {
    let h = harness();
    // An unknown `level` is a hard refusal. A malformed-looking filter is not:
    // "this is not a filter" arms with warnings, because almost anything is a
    // legal bare pattern — which is worth knowing when picking a failure to
    // test with.
    let bad = h.call(
        "collectors.add",
        json!({
            "name": "doomed",
            "filter": "ALL",
            "level": "bogus",
            "data": [{"path": "/Build/commit", "value": "should-not-land"}],
        }),
    );
    assert!(bad.error.is_some(), "the level must be refused: {bad:?}");

    let got = h.ok("domain_data.get", json!({}));
    assert!(
        !registry_paths(&got).contains(&"/Build/commit".to_string()),
        "a failed add must leave nothing behind: {got}"
    );
}
