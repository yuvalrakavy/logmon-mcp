//! Domain isolation — the load-bearing §9.1 three-site scoping (spec §11).
//!
//! A filter, trigger, or span trigger belonging to a session bound to one
//! domain must NOT affect another domain's records: it must not suppress the
//! other domain's storage, must not fire on its records, and must not receive
//! its span payloads. It must also not consume another domain's session's
//! post-window. These exercise the per-domain processors directly (no socket).

use chrono::Utc;
use logmon_broker_core::daemon::domain::{Domain, DomainConfig, DomainId, DomainSource};
use logmon_broker_core::daemon::log_processor::process_entry_for_domain;
use logmon_broker_core::daemon::session::{SessionId, SessionRegistry};
use logmon_broker_core::daemon::span_processor::process_span_for_domain;
use logmon_broker_core::gelf::message::{Level, LogEntry, LogSource};
use logmon_broker_core::span::types::*;
use std::collections::HashMap;
use std::sync::Arc;

fn domain(name: &str) -> Domain {
    Domain::new(
        DomainConfig {
            name: DomainId::new(name).unwrap(),
            log_buffer_size: 1000,
            span_buffer_size: 1000,
            source: DomainSource::Ephemeral,
        },
        0,
    )
}

fn make_entry(level: Level, msg: &str) -> LogEntry {
    LogEntry {
        seq: 0,
        timestamp: Utc::now(),
        level,
        message: msg.to_string(),
        full_message: None,
        host: "test".into(),
        facility: Some("app".into()),
        file: None,
        line: None,
        additional_fields: HashMap::new(),
        trace_id: None,
        span_id: None,
        matched_filters: Vec::new(),
        source: LogSource::Filter,
    }
}

fn make_span(name: &str, duration_ms: f64) -> SpanEntry {
    let now = Utc::now();
    SpanEntry {
        seq: 0,
        trace_id: 0xabc_u128,
        span_id: 0xdef_u64,
        parent_span_id: None,
        start_time: now,
        end_time: now,
        duration_ms,
        name: name.to_string(),
        kind: SpanKind::Internal,
        service_name: "test".to_string(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    }
}

/// Strip a session's auto-seeded default triggers so a test controls its
/// trigger set exactly.
fn clear_triggers(sessions: &SessionRegistry, sid: &SessionId) {
    for t in sessions.list_triggers(sid) {
        sessions.remove_trigger(sid, t.id).unwrap();
    }
}

/// Site 1 (`evaluate_filters`): a filter in domain B must not suppress storage
/// of domain A's non-matching logs. This is the cross-domain data-loss trap.
#[test]
fn filter_in_one_domain_does_not_drop_another_domains_logs() {
    let a = domain("a");
    let b = domain("b");
    let sessions = Arc::new(SessionRegistry::new());

    // A session bound to B with a filter that matches nothing here.
    let sb = sessions.create_anonymous();
    sessions.set_domain(&sb, b.id().clone());
    sessions.add_filter(&sb, "m=zzz_nomatch", None).unwrap();

    // An INFO log ingested on domain A. A has no filters → store-all; B's
    // filter must not suppress it.
    let mut info = make_entry(Level::Info, "hello from A");
    process_entry_for_domain(&mut info, &a.pipeline, &sessions, a.id());

    assert_eq!(
        a.pipeline.store_len(),
        1,
        "A's INFO must be stored despite B's non-matching filter"
    );
    assert_eq!(b.pipeline.store_len(), 0, "nothing should land in B");
}

/// Site 2 (`active_session_ids_sorted_by_pre_window`): a log trigger in domain
/// B must not fire on domain A's records.
#[test]
fn trigger_in_one_domain_does_not_fire_on_another_domains_logs() {
    let a = domain("a");
    let sessions = Arc::new(SessionRegistry::new());

    // Disconnected named session bound to B with a single matching trigger, so
    // a fire would QUEUE a notification we can observe.
    let sb = sessions.create_named("b").unwrap();
    clear_triggers(&sessions, &sb);
    sessions
        .add_trigger(&sb, "m=boom", 0, 0, 0, None, false)
        .unwrap();
    sessions.set_domain(&sb, DomainId::new("b").unwrap());
    sessions.disconnect(&sb);

    // Process a matching record in domain A. B's trigger must not fire.
    let mut err = make_entry(Level::Error, "boom in A");
    process_entry_for_domain(&mut err, &a.pipeline, &sessions, a.id());

    assert!(
        sessions.drain_notifications(&sb).is_empty(),
        "B's trigger must not fire on A's record"
    );
}

/// Site 3 (span `active_session_ids`): a span trigger in domain B must not
/// receive domain A's spans — this is the real cross-domain payload leak.
#[test]
fn span_trigger_in_one_domain_does_not_receive_another_domains_spans() {
    let a = domain("a");
    let sessions = Arc::new(SessionRegistry::new());

    let sb = sessions.create_named("b").unwrap();
    clear_triggers(&sessions, &sb);
    // d>=500 is a duration (span) selector.
    sessions
        .add_trigger(&sb, "d>=500", 0, 0, 0, None, false)
        .unwrap();
    sessions.set_domain(&sb, DomainId::new("b").unwrap());
    sessions.disconnect(&sb);

    // A slow span in domain A. B's span trigger must not receive it.
    let span = make_span("slow_A", 600.0);
    process_span_for_domain(&span, &a.span_store, &sessions, &a.pipeline, a.id());

    assert!(
        sessions.drain_notifications(&sb).is_empty(),
        "B's span trigger must not fire on A's span"
    );
}

/// The `post_window` counter of a session bound to A must not be consumed by
/// domain B's processor (two processors racing one session's counter).
#[test]
fn processing_one_domain_does_not_consume_another_domains_post_window() {
    let a = domain("a");
    let b = domain("b");
    let sessions = Arc::new(SessionRegistry::new());

    // Session bound to A with an active post-window of 1.
    let sa = sessions.create_anonymous();
    sessions.set_domain(&sa, a.id().clone());
    sessions.set_post_window(&sa, 1);

    // Process an entry in domain B. It must not touch A's session's window.
    let mut e = make_entry(Level::Info, "in B");
    process_entry_for_domain(&mut e, &b.pipeline, &sessions, b.id());

    assert!(
        sessions.decrement_post_window(&sa),
        "domain B's processor must not consume A's post-window"
    );
}

/// Baseline (Phase 1 store partitioning): domains have independent stores and
/// seq spaces — a record on A is invisible in B, and both first entries get
/// seq 1 without any cross-domain collision handling.
#[test]
fn domains_have_independent_stores_and_seq() {
    let a = domain("a");
    let b = domain("b");
    let sessions = Arc::new(SessionRegistry::new());

    let mut ea = make_entry(Level::Info, "a1");
    process_entry_for_domain(&mut ea, &a.pipeline, &sessions, a.id());
    let mut eb = make_entry(Level::Info, "b1");
    process_entry_for_domain(&mut eb, &b.pipeline, &sessions, b.id());

    assert_eq!(a.pipeline.store_len(), 1);
    assert_eq!(b.pipeline.store_len(), 1);
    assert_eq!(ea.seq, 1, "A's first entry is seq 1");
    assert_eq!(eb.seq, 1, "B's first entry is also seq 1 — separate spaces");
}
