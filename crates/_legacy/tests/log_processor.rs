use logmon_mcp_server::daemon::log_processor::{process_entry, sync_pre_buffer_size};
use logmon_mcp_server::daemon::session::SessionRegistry;
use logmon_mcp_server::engine::pipeline::LogPipeline;
use logmon_mcp_server::gelf::message::{Level, LogEntry, LogSource};
use chrono::Utc;
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
    sessions.add_filter(&sid, "l>=ERROR", Some("errors")).unwrap();
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

#[test]
fn test_post_window_skips_triggers() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    // Edit trigger to have post_window=2
    let triggers = sessions.list_triggers(&sid);
    // Edit both default triggers to post_window=2
    for t in &triggers {
        sessions
            .edit_trigger(&sid, t.id, None, None, Some(2), None, None)
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
            .edit_trigger(&sid, t.id, None, Some(0), None, None, None)
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
    let logs = pipeline.recent_logs(100, None);
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
    let logs = pipeline.recent_logs(100, None);
    let traced_logs: Vec<_> = logs.iter().filter(|l| l.trace_id == Some(trace)).collect();
    assert!(traced_logs.len() > 1); // error + at least some context
}
