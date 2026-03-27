use logmon_mcp_server::daemon::session::*;
use logmon_mcp_server::engine::pipeline::PipelineEvent;
use logmon_mcp_server::gelf::message::{Level, LogEntry, LogSource};
use std::collections::HashMap;

fn make_entry(level: Level, msg: &str, facility: Option<&str>) -> LogEntry {
    LogEntry {
        seq: 1,
        timestamp: chrono::Utc::now(),
        level,
        message: msg.to_string(),
        full_message: None,
        host: "test".into(),
        facility: facility.map(String::from),
        file: None,
        line: None,
        additional_fields: HashMap::new(),
        matched_filters: vec![],
        source: LogSource::Filter,
    }
}

#[test]
fn test_create_anonymous_session() {
    let registry = SessionRegistry::new();
    let id = registry.create_anonymous();
    assert!(registry.get(&id).is_some());
    assert!(registry.is_connected(&id));
}

#[test]
fn test_create_named_session() {
    let registry = SessionRegistry::new();
    let id = registry.create_named("store-debug").unwrap();
    assert_eq!(id, SessionId::Named("store-debug".to_string()));
    assert!(registry.is_connected(&id));
}

#[test]
fn test_invalid_session_name() {
    let registry = SessionRegistry::new();
    assert!(registry.create_named("../bad").is_err());
    assert!(registry.create_named("").is_err());
    assert!(registry.create_named("has spaces").is_err());
    assert!(registry.create_named("good-name_123").is_ok());
}

#[test]
fn test_reconnect_named_session() {
    let registry = SessionRegistry::new();
    let id = registry.create_named("test").unwrap();
    registry.disconnect(&id);
    assert!(!registry.is_connected(&id));
    registry.reconnect(&id).unwrap();
    assert!(registry.is_connected(&id));
}

#[test]
fn test_cannot_connect_to_active_session() {
    let registry = SessionRegistry::new();
    registry.create_named("test").unwrap();
    assert!(registry.create_named("test").is_err());
}

#[test]
fn test_anonymous_removed_on_disconnect() {
    let registry = SessionRegistry::new();
    let id = registry.create_anonymous();
    registry.disconnect(&id);
    assert!(registry.get(&id).is_none());
}

#[test]
fn test_named_persists_on_disconnect() {
    let registry = SessionRegistry::new();
    let id = registry.create_named("test").unwrap();
    registry.disconnect(&id);
    assert!(registry.get(&id).is_some());
    assert!(!registry.is_connected(&id));
}

#[test]
fn test_drop_named_session() {
    let registry = SessionRegistry::new();
    let id = registry.create_named("test").unwrap();
    registry.disconnect(&id);
    registry.drop_session("test").unwrap();
    assert!(registry.get(&id).is_none());
}

#[test]
fn test_list_sessions() {
    let registry = SessionRegistry::new();
    registry.create_anonymous();
    registry.create_named("alpha").unwrap();
    let list = registry.list();
    assert_eq!(list.len(), 2);
}

#[test]
fn test_default_triggers_per_session() {
    let registry = SessionRegistry::new();
    let id = registry.create_anonymous();
    let triggers = registry.list_triggers(&id);
    assert_eq!(triggers.len(), 2); // default error + panic triggers
}

#[test]
fn test_per_session_filters() {
    let registry = SessionRegistry::new();
    let id = registry.create_anonymous();
    let fid = registry.add_filter(&id, "fa=mqtt", Some("MQTT")).unwrap();
    let filters = registry.list_filters(&id);
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].description.as_deref(), Some("MQTT"));
    registry.remove_filter(&id, fid).unwrap();
    assert_eq!(registry.list_filters(&id).len(), 0);
}

#[test]
fn test_notification_queue() {
    let registry = SessionRegistry::new();
    let id = registry.create_named("test").unwrap();
    registry.disconnect(&id);

    let event = PipelineEvent {
        trigger_id: 1,
        trigger_description: None,
        filter_string: "l>=ERROR".to_string(),
        matched_entry: make_entry(Level::Error, "test", None),
        context_before: vec![],
        pre_trigger_flushed: 0,
        post_window_size: 0,
    };
    registry.queue_notification(&id, event);
    let queued = registry.drain_notifications(&id);
    assert_eq!(queued.len(), 1);
}

#[test]
fn test_post_window() {
    let registry = SessionRegistry::new();
    let id = registry.create_anonymous();
    registry.set_post_window(&id, 3);
    assert!(registry.any_post_window_active());
    assert!(registry.decrement_post_window(&id)); // 3->2, returns true
    assert!(registry.decrement_post_window(&id)); // 2->1
    assert!(registry.decrement_post_window(&id)); // 1->0
    assert!(!registry.decrement_post_window(&id)); // 0, returns false
    assert!(!registry.any_post_window_active());
}

#[test]
fn test_evaluate_filters_union() {
    let registry = SessionRegistry::new();
    let id_a = registry.create_anonymous();
    let id_b = registry.create_anonymous();
    registry.add_filter(&id_a, "fa=mqtt", Some("MQTT")).unwrap();
    registry
        .add_filter(&id_b, "l>=ERROR", Some("errors"))
        .unwrap();

    // Entry matching session A's filter
    let entry = make_entry(Level::Info, "mqtt msg", Some("app::mqtt"));
    let (should_store, descs) = registry.evaluate_filters(&entry);
    assert!(should_store);
    assert!(descs.contains(&"MQTT".to_string()));
}

#[test]
fn test_no_filters_means_store_all() {
    let registry = SessionRegistry::new();
    registry.create_anonymous(); // session with no filters

    let entry = make_entry(Level::Debug, "debug", None);
    let (should_store, _) = registry.evaluate_filters(&entry);
    assert!(should_store); // no filters = implicit ALL
}

#[test]
fn test_max_pre_window() {
    let registry = SessionRegistry::new();
    let id = registry.create_anonymous();
    // Default triggers have pre_window=500
    assert_eq!(registry.max_pre_window(), 500);
    registry
        .add_trigger(&id, "fa=test", 1000, 200, 5, None)
        .unwrap();
    assert_eq!(registry.max_pre_window(), 1000);
}

#[test]
fn test_session_removed_graceful() {
    let registry = SessionRegistry::new();
    let id = registry.create_anonymous();
    registry.disconnect(&id); // removes anonymous

    // All operations on removed session should return gracefully
    let entry = make_entry(Level::Error, "test", None);
    assert!(registry.evaluate_triggers(&id, &entry).is_empty());
    assert!(!registry.decrement_post_window(&id));
    assert!(registry.list_triggers(&id).is_empty());
    assert!(registry.list_filters(&id).is_empty());
}
