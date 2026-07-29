//! Collectors, exercised through the real span-processing path.
//!
//! The registry's own unit tests call `ingest_span` directly. These go through
//! `process_span_for_domain`, so they also prove the wiring — that a collector
//! is reached at all, and reached via its own pinned domain rather than via its
//! owner's current binding.

use chrono::{TimeZone, Utc};
use logmon_broker_core::collector::registry::CollectorRegistry;
use logmon_broker_core::collector::sample::Level;
use logmon_broker_core::collector::state::{CollectorDef, DEFAULT_MAX_SAMPLE_BYTES};
use logmon_broker_core::daemon::domain::DomainId;
use logmon_broker_core::daemon::session::{SessionId, SessionRegistry};
use logmon_broker_core::daemon::span_processor::process_span_for_domain;
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use logmon_broker_core::filter::parser::parse_filter;
use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::span::types::{SpanEntry, SpanKind, SpanStatus};
use std::collections::HashMap;
use std::sync::Arc;

const S: i64 = 1_700_000_000_000_000_000;

struct Harness {
    store: Arc<SpanStore>,
    sessions: Arc<SessionRegistry>,
    pipeline: Arc<LogPipeline>,
    collectors: Arc<CollectorRegistry>,
}

impl Harness {
    fn new() -> Self {
        Self {
            store: Arc::new(SpanStore::new(10_000, Arc::new(SeqCounter::new()))),
            sessions: Arc::new(SessionRegistry::new()),
            pipeline: Arc::new(LogPipeline::new(10_000)),
            collectors: Arc::new(CollectorRegistry::new()),
        }
    }

    fn feed(&self, domain: &DomainId, span: &SpanEntry) {
        process_span_for_domain(
            span,
            &self.store,
            &self.sessions,
            &self.pipeline,
            &self.collectors,
            domain,
        );
    }
}

fn def(name: &str, filter: &str, level: Level, group_keys: &[&str]) -> CollectorDef {
    CollectorDef {
        name: name.into(),
        filter_string: filter.into(),
        filter: parse_filter(filter).expect("valid filter"),
        level,
        group_keys: group_keys.iter().map(|s| s.to_string()).collect(),
        max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
        description: None,
    }
}

fn span(service: &str, name: &str, dur_ns: i64) -> SpanEntry {
    SpanEntry {
        seq: 0,
        trace_id: 42,
        span_id: 1,
        parent_span_id: None,
        start_time: Utc.timestamp_nanos(S),
        end_time: Utc.timestamp_nanos(S + dur_ns),
        duration_ms: dur_ns as f64 / 1e6,
        name: name.into(),
        kind: SpanKind::Internal,
        service_name: service.into(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    }
}

fn dom(n: &str) -> DomainId {
    DomainId::new(n).expect("valid domain")
}

#[test]
fn a_collector_receives_matching_spans_through_the_processing_path() {
    let h = Harness::new();
    let d = dom("t1");
    let c = h
        .collectors
        .add(
            &SessionId::Named("s".into()),
            &d,
            def("perf", "sv=store_server", Level::Tree, &[]),
            Utc::now(),
        )
        .expect("armed");

    h.feed(&d, &span("store_server", "reconcile", 5_000_000));
    h.feed(&d, &span("renderer", "paint", 1_000_000)); // wrong service
    h.feed(&d, &span("store_server", "commit", 3_000_000));

    let s = c.snapshot();
    assert_eq!(s.total.count, 2, "only matching spans are collected");
    assert_eq!(s.total.total_ns, 8_000_000);
    assert_eq!(s.per_name.len(), 2, "reconcile and commit");
    assert_eq!(s.samples.len(), 2, "and both are retained as samples");
}

#[test]
fn a_collector_is_deaf_to_other_domains() {
    // The isolation the whole domain feature exists for: a t1 collector must
    // never see a t2 span, however well it matches.
    let h = Harness::new();
    let (t1, t2) = (dom("t1"), dom("t2"));
    let c = h
        .collectors
        .add(
            &SessionId::Named("s".into()),
            &t1,
            def("perf", "ALL", Level::Timing, &[]),
            Utc::now(),
        )
        .expect("armed");

    h.feed(&t2, &span("svc", "op", 1_000));
    assert_eq!(c.snapshot().total.count, 0, "t2 traffic is invisible");

    h.feed(&t1, &span("svc", "op", 1_000));
    assert_eq!(c.snapshot().total.count, 1);
}

#[test]
fn the_owning_sessions_binding_does_not_move_the_collector() {
    // §4.4. The collector is pinned to the domain it was created in. Rebinding
    // its owning session must not silently redirect or stop it — that is the
    // failure that reaching collectors via `active_session_ids_for_domain`
    // would have reintroduced.
    let h = Harness::new();
    let (pinned, elsewhere) = (dom("t3"), dom("t9"));
    let owner = h.sessions.create_anonymous();
    let c = h
        .collectors
        .add(
            &owner,
            &pinned,
            def("perf", "ALL", Level::Timing, &[]),
            Utc::now(),
        )
        .expect("armed");

    h.feed(&pinned, &span("svc", "op", 1_000));
    assert_eq!(c.snapshot().total.count, 1);

    // The owner goes somewhere else entirely.
    h.sessions.set_domain(&owner, elsewhere.clone());

    h.feed(&pinned, &span("svc", "op", 1_000));
    assert_eq!(
        c.snapshot().total.count,
        2,
        "the collector still follows its own pin, not its owner's binding"
    );
    h.feed(&elsewhere, &span("svc", "op", 1_000));
    assert_eq!(
        c.snapshot().total.count,
        2,
        "and does not start following the owner either"
    );
}

#[test]
fn collectors_and_span_triggers_coexist_without_interfering() {
    // Both hang off the same ingest call. Neither should suppress the other,
    // and the collector must not depend on a session being bound.
    let h = Harness::new();
    let d = DomainId::default_domain();
    let sid = h.sessions.create_anonymous();
    let t = h
        .sessions
        .add_trigger(&sid, "d>=1", 0, 0, 0, Some("any span"), false)
        .expect("trigger added");
    let c = h
        .collectors
        .add(&sid, &d, def("perf", "ALL", Level::Tree, &[]), Utc::now())
        .expect("armed");

    h.feed(&d, &span("svc", "op", 5_000_000));

    assert_eq!(c.snapshot().total.count, 1, "the collector saw it");
    let fired = h
        .sessions
        .list_triggers(&sid)
        .into_iter()
        .find(|i| i.id == t)
        .expect("trigger present")
        .match_count;
    assert_eq!(fired, 1, "and so did the trigger");
}

#[test]
fn group_keys_split_an_ab_run_through_the_real_path() {
    // The driving workflow: one collector, both arms, split by an attribute
    // the system under test emits. The attribute is a BOOLEAN, which the span
    // matcher's `.as_str()` view cannot see — so this also proves the
    // collector reads attributes independently of the matcher.
    let h = Harness::new();
    let d = dom("ab");
    let c = h
        .collectors
        .add(
            &SessionId::Named("s".into()),
            &d,
            def("cache-ab", "sv=svc", Level::Timing, &["cache.enabled"]),
            Utc::now(),
        )
        .expect("armed");

    for (enabled, dur) in [(true, 1_000_000i64), (true, 1_200_000), (false, 5_000_000)] {
        let mut sp = span("svc", "resolve", dur);
        sp.attributes
            .insert("cache.enabled".into(), serde_json::json!(enabled));
        h.feed(&d, &sp);
    }

    let s = c.snapshot();
    assert_eq!(s.total.count, 3);
    assert_eq!(s.per_group.len(), 2, "one group per arm");

    let mut by_label: HashMap<&str, i128> = HashMap::new();
    for (k, stats) in &s.per_group {
        by_label.insert(s.group_values[0].resolve(k[0]), stats.total_ns);
    }
    assert_eq!(by_label.get("true"), Some(&2_200_000));
    assert_eq!(by_label.get("false"), Some(&5_000_000));
}
