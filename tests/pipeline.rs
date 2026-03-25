use gelf_mcp_server::engine::pipeline::LogPipeline;
use gelf_mcp_server::gelf::message::{LogEntry, Level, LogSource};
use chrono::Utc;
use std::collections::HashMap;

fn make_entry(seq: u64, level: Level, msg: &str) -> LogEntry {
    LogEntry {
        seq, timestamp: Utc::now(), level,
        message: msg.to_string(), full_message: None,
        host: "test".into(), facility: None, file: None, line: None,
        additional_fields: HashMap::new(),
        matched_filters: Vec::new(), source: LogSource::Filter,
    }
}

#[test]
fn test_normal_flow_no_filters_stores_everything() {
    let pipeline = LogPipeline::new(1000);
    let events = pipeline.process(make_entry(1, Level::Info, "hello"));
    assert_eq!(pipeline.store_len(), 1);
    assert!(events.is_empty());
}

#[test]
fn test_buffer_filter_blocks_non_matching() {
    let pipeline = LogPipeline::new(1000);
    pipeline.add_filter("l>=ERROR", Some("errors only")).unwrap();
    pipeline.process(make_entry(1, Level::Info, "hello"));
    assert_eq!(pipeline.store_len(), 0);
    pipeline.process(make_entry(2, Level::Error, "bad"));
    assert_eq!(pipeline.store_len(), 1);
}

#[test]
fn test_trigger_fires_and_flushes_pre_buffer() {
    let pipeline = LogPipeline::new(1000);
    // Add filter that blocks everything except errors
    pipeline.add_filter("l>=ERROR", None).unwrap();

    // Send 10 info messages (won't be stored due to filter, but go to pre-buffer)
    for i in 1..=10 {
        pipeline.process(make_entry(i, Level::Info, &format!("info {i}")));
    }
    assert_eq!(pipeline.store_len(), 0);

    // Send an error — default trigger fires, pre-buffer flushes
    let events = pipeline.process(make_entry(11, Level::Error, "crash!"));
    assert!(!events.is_empty());
    // Pre-buffer entries + the error itself should be in store
    assert!(pipeline.store_len() > 1);
}

#[test]
fn test_post_window_bypasses_filters_and_triggers() {
    let pipeline = LogPipeline::new(1000);
    pipeline.add_filter("l>=ERROR", None).unwrap();

    // Trigger fires on error
    pipeline.process(make_entry(1, Level::Error, "crash"));
    let before = pipeline.store_len();

    // Next entries during post-window should be stored despite filter
    pipeline.process(make_entry(2, Level::Info, "recovery step 1"));
    assert_eq!(pipeline.store_len(), before + 1);
}

#[test]
fn test_post_window_expires() {
    let pipeline = LogPipeline::new(1000);
    pipeline.add_filter("l>=ERROR", None).unwrap();

    // Edit default trigger to have post_window=2 for easy testing
    pipeline.edit_trigger(1, None, None, Some(2), None, None).unwrap();

    pipeline.process(make_entry(1, Level::Error, "crash"));
    let after_trigger = pipeline.store_len();

    // 2 post-window entries
    pipeline.process(make_entry(2, Level::Info, "post 1"));
    pipeline.process(make_entry(3, Level::Info, "post 2"));

    // 3rd should be filtered normally (post-window expired)
    pipeline.process(make_entry(4, Level::Info, "filtered out"));
    assert_eq!(pipeline.store_len(), after_trigger + 2);
}

#[test]
fn test_no_filters_means_store_everything() {
    let pipeline = LogPipeline::new(1000);
    // No filters = implicit ALL
    for i in 1..=5 {
        pipeline.process(make_entry(i, Level::Debug, "debug msg"));
    }
    assert_eq!(pipeline.store_len(), 5);
}

#[test]
fn test_seq_counter() {
    let pipeline = LogPipeline::new(1000);
    assert_eq!(pipeline.assign_seq(), 1);
    assert_eq!(pipeline.assign_seq(), 2);
    assert_eq!(pipeline.assign_seq(), 3);
}

#[test]
fn test_filter_crud() {
    let pipeline = LogPipeline::new(1000);
    let id = pipeline.add_filter("fa=mqtt", Some("MQTT")).unwrap();
    assert_eq!(pipeline.list_filters().len(), 1);
    pipeline.edit_filter(id, Some("fa=http"), Some("HTTP")).unwrap();
    let filters = pipeline.list_filters();
    assert_eq!(filters[0].description.as_deref(), Some("HTTP"));
    pipeline.remove_filter(id).unwrap();
    assert_eq!(pipeline.list_filters().len(), 0);
}

#[test]
fn test_pipeline_event_has_context() {
    let pipeline = LogPipeline::new(1000);
    pipeline.add_filter("l>=ERROR", None).unwrap();

    // Send some info messages first
    for i in 1..=5 {
        pipeline.process(make_entry(i, Level::Info, &format!("info {i}")));
    }

    // Error triggers — event should have context_before
    let events = pipeline.process(make_entry(6, Level::Error, "crash!"));
    assert!(!events.is_empty());
    assert!(!events[0].context_before.is_empty());
}

#[test]
fn test_matched_filter_descriptions_attached() {
    let pipeline = LogPipeline::new(1000);
    pipeline.add_filter("l>=WARN", Some("warnings+")).unwrap();

    pipeline.process(make_entry(1, Level::Warn, "warning msg"));
    let recent = pipeline.recent_logs(10, None);
    assert!(!recent.is_empty());
    assert!(recent[0].matched_filters.contains(&"warnings+".to_string()));
}

use std::sync::Arc;
use tokio::time::Duration;

#[tokio::test]
async fn test_end_to_end_udp_to_query() {
    let pipeline = Arc::new(gelf_mcp_server::engine::pipeline::LogPipeline::new(1000));

    let udp_handle = gelf_mcp_server::gelf::udp::start_udp_listener(
        "127.0.0.1:0", pipeline.clone()
    ).await.unwrap();

    // Send various GELF messages via UDP
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    for i in 0..20u64 {
        let level = if i % 5 == 0 { 3 } else { 6 }; // every 5th is ERROR (GELF level 3)
        let msg = serde_json::json!({
            "version": "1.1",
            "host": "e2e-test",
            "short_message": format!("message {i}"),
            "level": level,
            "facility": "test::e2e"
        });
        socket.send_to(
            msg.to_string().as_bytes(),
            format!("127.0.0.1:{}", udp_handle.port())
        ).unwrap();
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Verify all received
    assert_eq!(pipeline.store_len(), 20);

    // Query with filter — only errors (GELF level 3 = ERROR)
    let errors = pipeline.recent_logs(100, Some("l>=ERROR"));
    assert_eq!(errors.len(), 4); // messages 0, 5, 10, 15

    // Verify triggers fired for errors
    let triggers = pipeline.list_triggers();
    assert!(triggers[0].match_count > 0, "error trigger should have fired");

    // Test context query
    let all_logs = pipeline.recent_logs(100, None);
    let some_seq = all_logs[10].seq;
    let context = pipeline.context_by_seq(some_seq, 2, 2);
    assert!(context.len() >= 3); // at least before + target + after

    // Test clear
    let cleared = pipeline.clear_logs();
    assert_eq!(cleared, 20);
    assert_eq!(pipeline.store_len(), 0);
}
