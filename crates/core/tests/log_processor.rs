use chrono::Utc;
use logmon_broker_core::daemon::domain::DomainId;
use logmon_broker_core::daemon::log_processor::{process_entry, sync_pre_buffer_size};
use logmon_broker_core::daemon::session::{SessionId, SessionRegistry};
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::gelf::message::{Level, LogEntry, LogSource};
use std::collections::HashMap;
use std::sync::Arc;

fn make_entry(level: Level, msg: &str) -> LogEntry {
    LogEntry {
        seq: 0, // daemon assigns real seq
        timestamp: Utc::now(),
        level,
        message: msg.to_string(),
        full_message: None,
        host: "test".into(),
        facility: Some("test::module".into()),
        file: None,
        line: None,
        additional_fields: HashMap::new(),
        trace_id: None,
        span_id: None,
        matched_filters: Vec::new(),
        source: LogSource::Filter,
    }
}

fn make_entry_with_field(level: Level, msg: &str, key: &str, value: &str) -> LogEntry {
    let mut entry = make_entry(level, msg);
    entry.additional_fields.insert(
        key.to_string(),
        serde_json::Value::String(value.to_string()),
    );
    entry
}

fn match_count_of(sessions: &SessionRegistry, sid: &SessionId, trigger_id: u32) -> u64 {
    sessions
        .list_triggers(sid)
        .into_iter()
        .find(|t| t.id == trigger_id)
        .expect("trigger present")
        .match_count
}

/// REGRESSION: one trigger firing must not silence the others.
///
/// The post-window used to be a single counter on the SESSION, and the
/// processor skipped evaluating that whole session while it was positive. So
/// any trigger firing blinded every sibling for `post_window` entries — and a
/// frequently-matching trigger (`l>=ERROR` on a busy stream) starved the quiet
/// ones, which are exactly the triggers armed to catch something rare.
#[test]
fn a_busy_trigger_does_not_blind_a_quiet_one() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    // The shape you arm when hunting something rare.
    let quiet = sessions
        .add_trigger(&sid, "kind=needle", 5, 5, 5, Some("quiet"), false)
        .unwrap();
    sync_pre_buffer_size(&pipeline, &sessions);

    // The noisy built-in `l>=ERROR` trigger fires, opening its 200-entry window.
    let mut boom = make_entry(Level::Error, "boom");
    process_entry(&mut boom, &pipeline, &sessions);
    assert_eq!(
        match_count_of(&sessions, &sid, 1),
        1,
        "the busy trigger fired"
    );

    // The needle lands INSIDE that window.
    let mut needle = make_entry_with_field(Level::Info, "haystack", "kind", "needle");
    process_entry(&mut needle, &pipeline, &sessions);

    assert_eq!(
        match_count_of(&sessions, &sid, quiet),
        1,
        "a quiet trigger must still be evaluated while another trigger's post-window is open"
    );
}

/// REGRESSION: a small `post_window` must not truncate a larger one already
/// in flight.
///
/// The session's storage window used to be assigned with a plain `store`,
/// which was safe only because no trigger could match while a window was open.
/// Once triggers debounce individually a match CAN land mid-window, and a
/// `store` would then discard the aftermath a bigger trigger had asked for.
#[test]
fn a_short_window_trigger_does_not_truncate_a_longer_one() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    // INFO is not stored normally, so anything stored below is post-window capture.
    sessions
        .add_filter(&sid, "l>=ERROR", Some("errors"))
        .unwrap();
    // A deliberately impatient trigger: one entry of aftermath.
    sessions
        .add_trigger(&sid, "kind=needle", 0, 1, 0, Some("short"), false)
        .unwrap();
    sync_pre_buffer_size(&pipeline, &sessions);

    // The built-in l>=ERROR trigger fires: 200 entries of aftermath promised.
    let mut boom = make_entry(Level::Error, "boom");
    process_entry(&mut boom, &pipeline, &sessions);
    let after_boom = pipeline.store_len();

    // The impatient trigger matches INSIDE that window.
    let mut needle = make_entry_with_field(Level::Info, "haystack", "kind", "needle");
    process_entry(&mut needle, &pipeline, &sessions);

    // 10 more filtered entries — all still inside the ERROR trigger's window.
    for _ in 0..10 {
        let mut e = make_entry(Level::Info, "aftermath");
        process_entry(&mut e, &pipeline, &sessions);
    }

    // needle + 10 aftermath entries, all stored despite the l>=ERROR filter.
    assert_eq!(
        pipeline.store_len(),
        after_boom + 11,
        "a short post_window must extend, never shrink, a longer window in flight"
    );
}

/// The debounce we deliberately KEEP: a trigger does not re-fire inside the
/// window opened by its own match, so one burst yields one capture.
#[test]
fn a_trigger_does_not_refire_inside_its_own_post_window() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    let t = sessions
        .add_trigger(&sid, "kind=needle", 2, 2, 2, Some("debounced"), false)
        .unwrap();
    sync_pre_buffer_size(&pipeline, &sessions);

    for _ in 0..3 {
        let mut e = make_entry_with_field(Level::Info, "haystack", "kind", "needle");
        process_entry(&mut e, &pipeline, &sessions);
    }
    // Fired on #1, debounced through #2 and #3 (post_window = 2).
    assert_eq!(match_count_of(&sessions, &sid, t), 1);

    // Window has now drained, so the next match fires again.
    let mut e = make_entry_with_field(Level::Info, "haystack", "kind", "needle");
    process_entry(&mut e, &pipeline, &sessions);
    assert_eq!(match_count_of(&sessions, &sid, t), 2);
}

/// Shrinking `post_window` must take effect at once — notably `0`, which is the
/// documented way to ask for "count every match". The in-flight window drains
/// only on entries that reach `evaluate`, so leaving it stale can keep a trigger
/// debounced indefinitely on a quiet stream.
#[test]
fn editing_post_window_clamps_a_window_already_in_flight() {
    let sessions = SessionRegistry::new();
    let sid = sessions.create_anonymous();
    let t = sessions
        .add_trigger(&sid, "kind=needle", 0, 200, 0, Some("t"), false)
        .unwrap();
    let entry = make_entry_with_field(Level::Info, "haystack", "kind", "needle");

    assert_eq!(sessions.evaluate_triggers(&sid, &entry).len(), 1);
    assert!(
        sessions.evaluate_triggers(&sid, &entry).is_empty(),
        "debounced by its own 200-entry window"
    );

    sessions
        .edit_trigger(&sid, t, None, None, Some(0), None, None, None)
        .unwrap();
    assert_eq!(
        sessions.evaluate_triggers(&sid, &entry).len(),
        1,
        "post_window=0 must disarm the debounce immediately"
    );
}

/// F3: rebinding a session to another domain clears in-flight debounce windows.
/// The session's storage counter was always reset here; once firing suppression
/// moved per-trigger, those counters had to follow — otherwise a trigger is deaf
/// on its new domain because of the old domain's traffic.
#[test]
fn rebinding_a_session_clears_trigger_debounce_windows() {
    let sessions = SessionRegistry::new();
    let sid = sessions.create_anonymous();
    sessions
        .add_trigger(&sid, "kind=needle", 0, 200, 0, Some("t"), false)
        .unwrap();
    let entry = make_entry_with_field(Level::Info, "haystack", "kind", "needle");

    assert_eq!(sessions.evaluate_triggers(&sid, &entry).len(), 1);
    assert!(sessions.evaluate_triggers(&sid, &entry).is_empty());

    sessions.set_domain(&sid, DomainId::new("other").unwrap());
    assert_eq!(
        sessions.evaluate_triggers(&sid, &entry).len(),
        1,
        "a window opened by the old domain's traffic must not shape the new one"
    );
}

#[test]
fn test_log_stored_when_no_filters() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let _sid = sessions.create_anonymous();
    sync_pre_buffer_size(&pipeline, &sessions);

    let mut entry = make_entry(Level::Info, "hello");
    process_entry(&mut entry, &pipeline, &sessions);
    assert_eq!(pipeline.store_len(), 1);
    assert!(entry.seq > 0); // seq was assigned
}

#[test]
fn test_trigger_fires_and_flushes_pre_buffer() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    // Session has default error trigger (pre_window=500, post_window=200)

    // Add a filter so INFO logs are NOT stored normally
    sessions
        .add_filter(&sid, "l>=ERROR", Some("errors"))
        .unwrap();
    sync_pre_buffer_size(&pipeline, &sessions);

    // Send 10 INFO entries -- go to pre-buffer but not store
    for _ in 0..10 {
        let mut entry = make_entry(Level::Info, "background noise");
        process_entry(&mut entry, &pipeline, &sessions);
    }
    assert_eq!(pipeline.store_len(), 0);

    // Send ERROR -- trigger fires, pre-buffer flushes
    let mut error_entry = make_entry(Level::Error, "crash!");
    process_entry(&mut error_entry, &pipeline, &sessions);
    // Pre-buffer entries + triggering entry should be in store
    assert!(pipeline.store_len() > 1);
}

/// The post-window's STORAGE job: entries inside it are stored unconditionally,
/// bypassing the session's filters. (Renamed from `test_post_window_skips_triggers`
/// — the window no longer skips trigger evaluation; that suppression is now
/// per-trigger. This test only ever asserted the storage half.)
#[test]
fn test_post_window_stores_entries_bypassing_filters() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    // Edit trigger to have post_window=2
    let triggers = sessions.list_triggers(&sid);
    // Edit both default triggers to post_window=2
    for t in &triggers {
        sessions
            .edit_trigger(&sid, t.id, None, None, Some(2), None, None, None)
            .unwrap();
    }

    // Add filter to block INFO
    sessions.add_filter(&sid, "l>=ERROR", None).unwrap();
    sync_pre_buffer_size(&pipeline, &sessions);

    // Fire trigger
    let mut entry = make_entry(Level::Error, "crash");
    process_entry(&mut entry, &pipeline, &sessions);
    let after_trigger = pipeline.store_len();

    // 2 post-window entries -- stored despite filter
    let mut p1 = make_entry(Level::Info, "post 1");
    process_entry(&mut p1, &pipeline, &sessions);
    let mut p2 = make_entry(Level::Info, "post 2");
    process_entry(&mut p2, &pipeline, &sessions);
    assert_eq!(pipeline.store_len(), after_trigger + 2);

    // 3rd entry -- post-window expired, filtered out
    let mut p3 = make_entry(Level::Info, "filtered");
    process_entry(&mut p3, &pipeline, &sessions);
    assert_eq!(pipeline.store_len(), after_trigger + 2);
}

#[test]
fn test_per_session_filters() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid_a = sessions.create_anonymous();
    let sid_b = sessions.create_anonymous();

    sessions.add_filter(&sid_a, "fa=mqtt", None).unwrap();
    sessions.add_filter(&sid_b, "l>=ERROR", None).unwrap();
    sync_pre_buffer_size(&pipeline, &sessions);

    // MQTT INFO -> stored (matches A's filter)
    let mut e1 = make_entry(Level::Info, "mqtt msg");
    e1.facility = Some("app::mqtt".into());
    process_entry(&mut e1, &pipeline, &sessions);
    assert_eq!(pipeline.store_len(), 1);

    // Non-MQTT DEBUG -> not stored
    let mut e2 = make_entry(Level::Debug, "debug msg");
    e2.facility = Some("app::http".into());
    process_entry(&mut e2, &pipeline, &sessions);
    assert_eq!(pipeline.store_len(), 1);

    // ERROR -> stored (matches B's filter, and also triggers default trigger)
    let mut e3 = make_entry(Level::Error, "error msg");
    e3.facility = Some("app::http".into());
    process_entry(&mut e3, &pipeline, &sessions);
    assert!(pipeline.store_len() > 1); // error + possible pre-buffer flush
}

#[test]
fn test_notification_queued_for_disconnected_session() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_named("test-queue").unwrap();
    sync_pre_buffer_size(&pipeline, &sessions);
    sessions.disconnect(&sid);

    let mut entry = make_entry(Level::Error, "crash while disconnected");
    process_entry(&mut entry, &pipeline, &sessions);

    let queued = sessions.drain_notifications(&sid);
    assert_eq!(queued.len(), 1);
}

#[test]
fn test_zero_sessions_stores_everything() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    // No sessions at all

    let mut entry = make_entry(Level::Debug, "nobody listening");
    process_entry(&mut entry, &pipeline, &sessions);
    // No filters from any session = implicit ALL
    assert_eq!(pipeline.store_len(), 1);
}

#[test]
fn test_trigger_with_pre_window_zero() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    // Edit default triggers to pre_window=0
    let triggers = sessions.list_triggers(&sid);
    for t in &triggers {
        sessions
            .edit_trigger(&sid, t.id, None, Some(0), None, None, None, None)
            .unwrap();
    }

    // Send some context then an error
    for _ in 0..5 {
        let mut e = make_entry(Level::Info, "context");
        process_entry(&mut e, &pipeline, &sessions);
    }
    let mut error = make_entry(Level::Error, "crash");
    process_entry(&mut error, &pipeline, &sessions);

    // Triggering entry must be stored even with pre_window=0
    let logs = pipeline.recent_logs(100, None, false);
    assert!(logs.iter().any(|l| l.message == "crash"));
}

#[test]
fn test_session_removed_during_processing_no_panic() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();

    // Remove the session (anonymous sessions are removed on disconnect)
    sessions.disconnect(&sid);

    // This should not panic -- process_entry skips missing sessions
    let mut entry = make_entry(Level::Error, "nobody home");
    process_entry(&mut entry, &pipeline, &sessions);
    // Entry stored via "no sessions = no filters = store everything" path
    assert_eq!(pipeline.store_len(), 1);
}

#[test]
fn test_trace_aware_prebuffer_flush() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    sessions.add_filter(&sid, "l>=ERROR", None).unwrap();
    sync_pre_buffer_size(&pipeline, &sessions);

    let trace = 0xabc_u128;

    // Send INFO logs with trace context — not stored (filter blocks INFO)
    for i in 0..5 {
        let mut e = make_entry(Level::Info, &format!("step {i}"));
        e.trace_id = Some(trace);
        process_entry(&mut e, &pipeline, &sessions);
    }
    assert_eq!(pipeline.store_len(), 0);

    // Send ERROR with same trace_id — trigger fires
    let mut error = make_entry(Level::Error, "crash");
    error.trace_id = Some(trace);
    process_entry(&mut error, &pipeline, &sessions);

    // Pre-buffer flush + trace-aware flush should include the traced INFO logs
    let logs = pipeline.recent_logs(100, None, false);
    let traced_logs: Vec<_> = logs.iter().filter(|l| l.trace_id == Some(trace)).collect();
    assert!(traced_logs.len() > 1); // error + at least some context
}
