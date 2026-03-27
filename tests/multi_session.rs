use logmon_mcp_server::daemon::log_processor::{process_entry, sync_pre_buffer_size};
use logmon_mcp_server::daemon::session::SessionRegistry;
use logmon_mcp_server::engine::pipeline::LogPipeline;
use logmon_mcp_server::gelf::message::{LogEntry, Level, LogSource};
use logmon_mcp_server::filter::parser::parse_filter;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;

fn make_entry(level: Level, msg: &str, facility: &str) -> LogEntry {
    LogEntry {
        seq: 0, timestamp: Utc::now(), level,
        message: msg.to_string(), full_message: None,
        host: "test".into(), facility: Some(facility.into()),
        file: None, line: None,
        additional_fields: HashMap::new(),
        matched_filters: Vec::new(), source: LogSource::Filter,
    }
}

#[test]
fn test_two_sessions_share_logs() {
    // Create two sessions, send a log, both can see it
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let _sid_a = sessions.create_anonymous();
    let _sid_b = sessions.create_anonymous();
    sync_pre_buffer_size(&pipeline, &sessions);

    let mut entry = make_entry(Level::Info, "shared log", "app::main");
    process_entry(&mut entry, &pipeline, &sessions);

    let filter = parse_filter("ALL").unwrap();
    let logs = pipeline.recent_logs(10, Some(&filter));
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "shared log");
}

#[test]
fn test_per_session_filter_lens() {
    // Session A filters for mqtt, Session B for http
    // Both logs stored (union), but each session's lens sees only their subset
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid_a = sessions.create_anonymous();
    let sid_b = sessions.create_anonymous();
    sessions.add_filter(&sid_a, "fa=mqtt", None).unwrap();
    sessions.add_filter(&sid_b, "fa=http", None).unwrap();
    sync_pre_buffer_size(&pipeline, &sessions);

    let mut mqtt_entry = make_entry(Level::Info, "mqtt msg", "app::mqtt");
    process_entry(&mut mqtt_entry, &pipeline, &sessions);
    let mut http_entry = make_entry(Level::Info, "http msg", "app::http");
    process_entry(&mut http_entry, &pipeline, &sessions);

    assert_eq!(pipeline.store_len(), 2);

    let filter_a = parse_filter("fa=mqtt").unwrap();
    let logs_a = pipeline.recent_logs(10, Some(&filter_a));
    assert_eq!(logs_a.len(), 1);
    assert_eq!(logs_a[0].message, "mqtt msg");

    let filter_b = parse_filter("fa=http").unwrap();
    let logs_b = pipeline.recent_logs(10, Some(&filter_b));
    assert_eq!(logs_b.len(), 1);
    assert_eq!(logs_b[0].message, "http msg");
}

#[test]
fn test_named_session_survives_disconnect() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_named("persistent").unwrap();

    sessions.add_trigger(&sid, "fa=special", 100, 50, 3, Some("custom")).unwrap();
    let triggers_before = sessions.list_triggers(&sid);
    assert_eq!(triggers_before.len(), 3); // 2 defaults + 1 custom

    sessions.disconnect(&sid);
    assert!(!sessions.is_connected(&sid));

    let triggers_after = sessions.list_triggers(&sid);
    assert_eq!(triggers_after.len(), 3);

    sessions.reconnect(&sid).unwrap();
    assert!(sessions.is_connected(&sid));

    // suppress unused variable warning
    let _ = pipeline;
}

#[test]
fn test_anonymous_session_cleanup_on_disconnect() {
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    assert_eq!(sessions.list().len(), 1);
    sessions.disconnect(&sid);
    assert_eq!(sessions.list().len(), 0);
}
