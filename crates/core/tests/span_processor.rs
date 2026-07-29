use chrono::Utc;
use logmon_broker_core::daemon::session::{SessionId, SessionRegistry};
use logmon_broker_core::daemon::span_processor::process_span;
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::span::types::*;
use std::collections::HashMap;
use std::sync::Arc;

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

fn match_count_of(sessions: &SessionRegistry, sid: &SessionId, trigger_id: u32) -> u64 {
    sessions
        .list_triggers(sid)
        .into_iter()
        .find(|t| t.id == trigger_id)
        .expect("trigger present")
        .match_count
}

#[test]
fn test_span_stored() {
    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(100, seq));
    let sessions = Arc::new(SessionRegistry::new());
    let pipeline = Arc::new(LogPipeline::new(100));

    let span = make_span("query", 100.0);
    process_span(&span, &store, &sessions, &pipeline);
    assert_eq!(store.len(), 1);
}

#[test]
fn test_span_trigger_fires() {
    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(100, seq));
    let sessions = Arc::new(SessionRegistry::new());
    let pipeline = Arc::new(LogPipeline::new(100));
    let sid = sessions.create_anonymous();

    // Add a span trigger (d>=500 is a span selector, not log)
    let t = sessions
        .add_trigger(&sid, "d>=500", 0, 0, 0, Some("slow span"), false)
        .unwrap();

    let span = make_span("slow_query", 600.0);
    process_span(&span, &store, &sessions, &pipeline);

    // Span stored
    assert_eq!(store.len(), 1);
    // ...and the trigger actually FIRED. `store.len()` alone proves nothing
    // here: process_span stores every span unconditionally at step 1, so this
    // assertion held even when no trigger matched. match_count is the first
    // signal on this path that distinguishes fired from didn't.
    assert_eq!(match_count_of(&sessions, &sid, t), 1);
}

#[test]
fn span_triggers_are_not_debounced() {
    // Span triggers do NOT observe `post_window` — a matching burst fires once
    // per span, not once per window. This is the behaviour the log path does
    // not share (`TriggerManager::evaluate` decrements `post_remaining` and
    // skips), and it is deliberately preserved by `evaluate_span`.
    //
    // Pinned because nothing else pins it: `evaluate_span` was written by
    // mirroring `evaluate`, and copying two more lines from it would silently
    // introduce debouncing here with every existing test still green.
    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(100, seq));
    let sessions = Arc::new(SessionRegistry::new());
    let pipeline = Arc::new(LogPipeline::new(100));
    let sid = sessions.create_anonymous();

    // A post_window of 50 would blind a LOG trigger for the next 50 entries.
    let t = sessions
        .add_trigger(&sid, "d>=500", 0, 50, 0, Some("slow span"), false)
        .unwrap();

    for _ in 0..3 {
        let span = make_span("slow_query", 600.0);
        process_span(&span, &store, &sessions, &pipeline);
    }

    assert_eq!(
        match_count_of(&sessions, &sid, t),
        3,
        "every matching span fires; a post_window must not suppress spans"
    );
}

#[test]
fn non_span_filter_trigger_never_fires_on_a_span() {
    // `ALL` is the sharp case for the is_span_filter guard: `matches_span`
    // returns TRUE for `ParsedFilter::All`, so without the guard an `ALL`
    // trigger would fire on every span in the domain. `is_span_filter` returns
    // false for it, so it does not.
    //
    // This pins shipped behaviour, not an endorsement of it — the same
    // `is_span_filter` gap is why the span-time-collector spec (§7) rejects
    // reusing this predicate as a filter-admission gate.
    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(100, seq));
    let sessions = Arc::new(SessionRegistry::new());
    let pipeline = Arc::new(LogPipeline::new(100));
    let sid = sessions.create_anonymous();

    let t_all = sessions
        .add_trigger(&sid, "ALL", 0, 0, 0, Some("everything"), false)
        .unwrap();

    let span = make_span("anything", 1.0);
    process_span(&span, &store, &sessions, &pipeline);

    assert_eq!(store.len(), 1, "the span was ingested");
    assert_eq!(
        match_count_of(&sessions, &sid, t_all),
        0,
        "a non-span filter must not fire on a span even when it matches"
    );
    // The seeded log trigger `l>=ERROR` (id 1) likewise stays silent.
    assert_eq!(
        match_count_of(&sessions, &sid, 1),
        0,
        "a level filter is log-only"
    );
}

#[test]
fn oneshot_span_trigger_is_removed_after_it_fires() {
    // The oneshot arm of the span loop. `oneshot_triggers.rs` covers the log
    // path only — every one of its tests stayed green with span evaluation
    // stubbed out entirely.
    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(100, seq));
    let sessions = Arc::new(SessionRegistry::new());
    let pipeline = Arc::new(LogPipeline::new(100));
    let sid = sessions.create_anonymous();

    let present = |id: u32| sessions.list_triggers(&sid).iter().any(|i| i.id == id);

    let t = sessions
        .add_trigger(&sid, "d>=500", 0, 0, 0, Some("once"), true)
        .unwrap();
    assert!(present(t), "armed");

    // Control arm: a NON-matching span must leave it armed. Without this the
    // test below would pass against an implementation that removed oneshot
    // triggers unconditionally.
    process_span(&make_span("fast", 10.0), &store, &sessions, &pipeline);
    assert!(present(t), "a non-matching span must not consume a oneshot");

    process_span(&make_span("slow", 600.0), &store, &sessions, &pipeline);
    assert!(
        !present(t),
        "a oneshot span trigger is removed once a span matches it"
    );

    // Removal is the only observable evidence that it fired: `match_count` is
    // unreadable afterwards because the trigger it lives on is gone.
}

#[test]
fn test_span_trigger_no_match() {
    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(100, seq));
    let sessions = Arc::new(SessionRegistry::new());
    let pipeline = Arc::new(LogPipeline::new(100));
    let sid = sessions.create_anonymous();

    let t = sessions
        .add_trigger(&sid, "d>=500", 0, 0, 0, Some("slow"), false)
        .unwrap();

    let span = make_span("fast_query", 10.0);
    process_span(&span, &store, &sessions, &pipeline);

    // Span stored but no trigger fired
    assert_eq!(store.len(), 1);
    assert_eq!(
        match_count_of(&sessions, &sid, t),
        0,
        "a non-matching span must not count as a fire"
    );
}
