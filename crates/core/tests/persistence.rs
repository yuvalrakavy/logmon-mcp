use logmon_broker_core::daemon::persistence::*;
use std::path::PathBuf;

#[test]
fn test_state_default() {
    let state = DaemonState::default();
    assert_eq!(state.seq_block, 0);
    assert!(state.named_sessions.is_empty());
}

#[test]
fn test_state_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut state = DaemonState::default();
    state.seq_block = 49000;
    state.named_sessions.insert("test".to_string(), PersistedSession {
        triggers: vec![PersistedTrigger {
            filter: "l>=ERROR".to_string(),
            pre_window: 500, post_window: 200, notify_context: 5,
            description: Some("error trigger".to_string()),
            oneshot: false,
        }],
        filters: vec![PersistedFilter {
            filter: "fa=mqtt".to_string(),
            description: Some("mqtt only".to_string()),
        }],
        client_info: None,
        bookmarks: vec![],
    });
    save_state(&path, &state).unwrap();
    let loaded = load_state(&path).unwrap();
    assert_eq!(loaded.seq_block, 49000);
    assert_eq!(loaded.named_sessions.len(), 1);
    assert_eq!(loaded.named_sessions["test"].triggers.len(), 1);
    assert_eq!(loaded.named_sessions["test"].filters.len(), 1);
}

#[test]
fn test_persisted_trigger_roundtrip_with_oneshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut state = DaemonState::default();
    state.named_sessions.insert(
        "named".to_string(),
        PersistedSession {
            triggers: vec![PersistedTrigger {
                filter: "l>=ERROR".to_string(),
                pre_window: 0,
                post_window: 0,
                notify_context: 0,
                description: None,
                oneshot: true,
            }],
            filters: vec![],
            client_info: None,
            bookmarks: vec![],
        },
    );
    save_state(&path, &state).unwrap();
    let loaded = load_state(&path).unwrap();
    assert!(loaded.named_sessions["named"].triggers[0].oneshot);
}

#[test]
fn test_persisted_trigger_oneshot_serde_default() {
    // state.json files written before oneshot landed must still load with
    // oneshot defaulting to false.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let legacy = r#"{
        "seq_block": 0,
        "named_sessions": {
            "named": {
                "triggers": [{
                    "filter": "l>=ERROR",
                    "pre_window": 0,
                    "post_window": 0,
                    "notify_context": 0,
                    "description": null
                }],
                "filters": []
            }
        }
    }"#;
    std::fs::write(&path, legacy).unwrap();
    let loaded = load_state(&path).unwrap();
    assert!(!loaded.named_sessions["named"].triggers[0].oneshot);
}

#[test]
fn test_state_missing_file_returns_default() {
    let path = PathBuf::from("/tmp/nonexistent_logmon_test/state.json");
    let state = load_state(&path).unwrap();
    assert_eq!(state.seq_block, 0);
}

#[test]
fn test_config_default() {
    let config = DaemonConfig::default();
    assert_eq!(config.gelf_port, 12201);
    assert_eq!(config.buffer_size, 10000);
    assert_eq!(config.idle_timeout_secs, 1800);
    assert!(!config.persist_buffer_on_exit);
}

#[test]
fn test_config_missing_file_returns_default() {
    let path = PathBuf::from("/tmp/nonexistent_logmon_test/config.json");
    let config = load_config(&path).unwrap();
    assert_eq!(config.gelf_port, 12201);
}

#[test]
fn test_config_partial_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, r#"{"gelf_port": 9999}"#).unwrap();
    let config = load_config(&path).unwrap();
    assert_eq!(config.gelf_port, 9999);
    assert_eq!(config.buffer_size, 10000); // default
}

#[test]
fn test_config_dir() {
    let dir = config_dir();
    assert!(dir.to_string_lossy().contains("logmon"));
}

#[test]
fn test_seq_block_size_constant() {
    assert_eq!(SEQ_BLOCK_SIZE, 1000);
}

#[test]
fn test_legacy_bookmarks_with_timestamp_field_are_dropped() {
    // Pre-cursor state.json had `timestamp` instead of `seq` on each bookmark.
    // The lenient deserializer should drop the malformed array (logging a WARN)
    // and leave the rest of the session intact.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let legacy = r#"{
        "seq_block": 1000,
        "named_sessions": {
            "test_session": {
                "triggers": [],
                "filters": [],
                "client_info": null,
                "bookmarks": [{
                    "name": "old",
                    "timestamp": "2026-04-30T12:00:00Z",
                    "description": "from before cursor migration"
                }]
            }
        }
    }"#;
    std::fs::write(&path, legacy).unwrap();
    let loaded = load_state(&path).unwrap();
    let session = &loaded.named_sessions["test_session"];
    assert!(
        session.bookmarks.is_empty(),
        "legacy bookmarks should be dropped, got {:?}",
        session.bookmarks
    );
}

#[test]
fn test_bookmarks_roundtrip_with_seq_field() {
    // New-shape bookmarks survive a full save/load cycle.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut state = DaemonState::default();
    state.named_sessions.insert(
        "named".to_string(),
        PersistedSession {
            triggers: vec![],
            filters: vec![],
            client_info: None,
            bookmarks: vec![PersistedBookmark {
                name: "anchor".to_string(),
                seq: 42,
                created_at: chrono::Utc::now(),
                description: Some("preserved".to_string()),
            }],
        },
    );
    save_state(&path, &state).unwrap();
    let loaded = load_state(&path).unwrap();
    let bookmarks = &loaded.named_sessions["named"].bookmarks;
    assert_eq!(bookmarks.len(), 1);
    assert_eq!(bookmarks[0].name, "anchor");
    assert_eq!(bookmarks[0].seq, 42);
    assert_eq!(bookmarks[0].description.as_deref(), Some("preserved"));
}
