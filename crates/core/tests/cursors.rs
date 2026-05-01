//! End-to-end tests for the seq-based bookmark protocol introduced by the
//! cursor design (Task 2). The cursor primitive itself (`c>=`) ships in a
//! later task; this file only exercises the storage + protocol + handler
//! refactor.

#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{
    BookmarksAddResult, BookmarksListResult,
};
use serde_json::json;
use std::fs;

#[tokio::test]
async fn add_bookmark_with_explicit_start_seq() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let added: BookmarksAddResult = client
        .call(
            "bookmarks.add",
            json!({
                "name": "anchor",
                "start_seq": 42,
                "description": "explicit start",
            }),
        )
        .await
        .unwrap();
    assert_eq!(added.seq, 42);

    let list: BookmarksListResult = client
        .call("bookmarks.list", json!({}))
        .await
        .unwrap();
    let entry = list
        .bookmarks
        .iter()
        .find(|b| b.qualified_name.ends_with("/anchor"))
        .expect("expected to find /anchor in list");
    assert_eq!(entry.seq, 42);
    assert_eq!(entry.description.as_deref(), Some("explicit start"));
}

#[tokio::test]
async fn add_bookmark_replace_overwrites() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let _: BookmarksAddResult = client
        .call(
            "bookmarks.add",
            json!({ "name": "x", "start_seq": 1 }),
        )
        .await
        .unwrap();

    // Without `replace: true`, a duplicate must error.
    let err: Result<BookmarksAddResult, _> = client
        .call(
            "bookmarks.add",
            json!({ "name": "x", "start_seq": 2 }),
        )
        .await;
    assert!(err.is_err());

    // With `replace: true`, the same name overwrites at a fresh seq.
    let _: BookmarksAddResult = client
        .call(
            "bookmarks.add",
            json!({ "name": "x", "start_seq": 2, "replace": true }),
        )
        .await
        .unwrap();

    let list: BookmarksListResult = client
        .call("bookmarks.list", json!({}))
        .await
        .unwrap();
    let entry = list
        .bookmarks
        .iter()
        .find(|b| b.qualified_name.ends_with("/x"))
        .expect("expected to find /x in list");
    assert_eq!(entry.seq, 2);
}

#[tokio::test]
async fn legacy_state_json_with_old_bookmark_shape_warns_and_continues() {
    // Spawn a daemon, get its tempdir, shut down, write a legacy-shape state.json,
    // restart, verify daemon starts and bookmarks are dropped.
    let daemon = spawn_test_daemon().await;
    let tempdir = daemon.tempdir.clone();
    let tempdir_path = tempdir.path().to_path_buf();
    daemon.shutdown().await;
    drop(daemon);

    let legacy = json!({
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
    });
    fs::write(
        tempdir_path.join("state.json"),
        serde_json::to_string_pretty(&legacy).unwrap(),
    )
    .unwrap();

    // Re-spawn with same tempdir using the single-arg public API.
    let daemon = TestDaemonHandle::spawn_in_tempdir(tempdir).await;
    let mut client = daemon.connect_named("test_session", None).await;

    // Daemon survived deserialize; bookmarks should be empty for the restored session.
    let list: BookmarksListResult = client.call("bookmarks.list", json!({})).await.unwrap();
    assert!(
        list.bookmarks
            .iter()
            .all(|b| !b.qualified_name.starts_with("test_session/old")),
        "legacy bookmarks should have been discarded; got {:?}",
        list.bookmarks
    );
}

#[tokio::test]
async fn bookmark_persists_across_restart() {
    // Verify the persistence round-trip works for seq-based bookmarks via the
    // pure-read b>= path. (Cursor-specific cross-restart behavior is tested
    // in Task 15 once c>= and cursor_advanced_to are wired.)
    let mut daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_named("persist", None).await;

    // Capture pre-add and post-add times to verify created_at survives the restart.
    let pre_add = chrono::Utc::now();
    let added: BookmarksAddResult = client
        .call(
            "bookmarks.add",
            json!({
                "name": "anchor",
                "start_seq": 42,
                "description": "preserved",
            }),
        )
        .await
        .unwrap();
    let post_add = chrono::Utc::now();
    assert_eq!(added.seq, 42);

    // Drop client, restart daemon, reconnect to same named session.
    drop(client);
    daemon.restart().await;
    let mut client = daemon.connect_named("persist", None).await;

    // Bookmark survives with original seq, description, AND created_at.
    let list: BookmarksListResult = client.call("bookmarks.list", json!({})).await.unwrap();
    let entry = list
        .bookmarks
        .iter()
        .find(|b| b.qualified_name == "persist/anchor")
        .expect("anchor bookmark should survive restart");
    assert_eq!(entry.seq, 42);
    assert_eq!(entry.description.as_deref(), Some("preserved"));
    assert!(
        entry.created_at >= pre_add && entry.created_at <= post_add,
        "created_at should be preserved from pre-restart; got {:?}, pre_add {:?}, post_add {:?}",
        entry.created_at,
        pre_add,
        post_add
    );
}

#[tokio::test]
async fn c_ge_rejected_in_traces_recent() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let result: Result<serde_json::Value, _> = client.call("traces.recent", json!({
        "filter": "c>=mycur"
    })).await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cursor qualifier not permitted"), "got: {err}");
}

#[tokio::test]
async fn c_ge_rejected_in_traces_get() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let result: Result<serde_json::Value, _> = client.call("traces.get", json!({
        "trace_id": "00000000000000000000000000000001",
        "filter": "c>=mycur"
    })).await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cursor qualifier not permitted"), "got: {err}");
}

#[tokio::test]
async fn c_ge_rejected_in_traces_slow() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let result: Result<serde_json::Value, _> = client.call("traces.slow", json!({
        "filter": "c>=mycur"
    })).await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cursor qualifier not permitted"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Tasks 10/11/12: cursor commit + oldest-first ordering through the allow-list
// handlers (logs.recent / logs.export / traces.logs).
// ---------------------------------------------------------------------------

use logmon_broker_core::gelf::message::Level;
use logmon_broker_protocol::{LogsExportResult, LogsRecentResult, TracesLogsResult};

#[tokio::test]
async fn cursor_advances_and_paginates_oldest_first() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    for i in 0..5 {
        daemon.inject_log(Level::Info, &format!("record-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // First cursor read with count=3 — returns oldest 3, advances cursor.
    let r1: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "count": 3,
                "filter": "c>=cur",
            }),
        )
        .await
        .unwrap();
    assert_eq!(r1.logs.len(), 3);
    assert_eq!(r1.logs[0].message, "record-0"); // oldest first
    assert_eq!(r1.logs[1].message, "record-1");
    assert_eq!(r1.logs[2].message, "record-2");
    assert!(r1.cursor_advanced_to.is_some());

    // Second cursor read — returns next 2.
    let r2: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "count": 3,
                "filter": "c>=cur",
            }),
        )
        .await
        .unwrap();
    assert_eq!(r2.logs.len(), 2);
    assert_eq!(r2.logs[0].message, "record-3");
    assert_eq!(r2.logs[1].message, "record-4");

    // Third cursor read — empty, no advance.
    let r3: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "count": 3,
                "filter": "c>=cur",
            }),
        )
        .await
        .unwrap();
    assert!(r3.logs.is_empty());
    assert_eq!(r3.cursor_advanced_to, None);
}

#[tokio::test]
async fn no_cursor_returns_newest_first_unchanged() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    for i in 0..3 {
        daemon.inject_log(Level::Info, &format!("record-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let r: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "count": 5,
            }),
        )
        .await
        .unwrap();
    assert_eq!(r.logs.len(), 3);
    assert_eq!(r.logs[0].message, "record-2"); // newest first
    assert_eq!(r.logs[2].message, "record-0");
    assert_eq!(r.cursor_advanced_to, None);
}

#[tokio::test]
async fn export_with_cursor_advances_and_returns_oldest_first() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    for i in 0..5 {
        daemon.inject_log(Level::Info, &format!("export-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let r: LogsExportResult = client
        .call(
            "logs.export",
            json!({
                "filter": "c>=expcur",
                "count": 100,
            }),
        )
        .await
        .unwrap();
    assert_eq!(r.logs.len(), 5);
    assert_eq!(r.logs[0].message, "export-0");
    assert!(r.cursor_advanced_to.is_some());

    let r2: LogsExportResult = client
        .call(
            "logs.export",
            json!({
                "filter": "c>=expcur",
                "count": 100,
            }),
        )
        .await
        .unwrap();
    assert!(r2.logs.is_empty());
    assert_eq!(r2.cursor_advanced_to, None);
}

#[tokio::test]
async fn traces_logs_with_cursor_field_present() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // Use a trace_id that no logs have. Result is empty; cursor_advanced_to is None.
    // The point of this test is just that the field exists on the wire.
    let r: TracesLogsResult = client
        .call(
            "traces.logs",
            json!({
                "trace_id": "00000000000000000000000000000001",
                "filter": "c>=tlcur",
            }),
        )
        .await
        .unwrap();
    assert!(r.logs.is_empty());
    assert_eq!(r.cursor_advanced_to, None);
}

// ---------------------------------------------------------------------------
// Task 13: anonymous session disconnect drops bookmarks
// ---------------------------------------------------------------------------

#[test]
fn bookmark_store_clear_session_removes_matching() {
    use logmon_broker_core::store::bookmarks::BookmarkStore;
    let store = BookmarkStore::new();

    let (_b1, _) = store.add("session1", "name1", 0, None, false).unwrap();
    let (_b2, _) = store.add("session1", "name2", 1, None, false).unwrap();
    let (_b3, _) = store.add("session2", "name3", 2, None, false).unwrap();

    assert_eq!(store.list().len(), 3);

    // Clear session1
    let removed = store.clear_session("session1");
    assert_eq!(removed, 2);
    assert_eq!(store.list().len(), 1);
    assert_eq!(store.list()[0].qualified_name, "session2/name3");
}

#[tokio::test]
async fn anonymous_session_disconnect_drops_bookmarks() {
    let daemon = spawn_test_daemon().await;

    {
        let mut client = daemon.connect_anon().await;
        let _: BookmarksAddResult = client
            .call(
                "bookmarks.add",
                json!({
                    "name": "ephemeral"
                }),
            )
            .await
            .unwrap();
        // Explicitly close the connection to trigger EOF on the server side
        client.close().await.unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut client = daemon.connect_anon().await;
    let list: BookmarksListResult = client.call("bookmarks.list", json!({})).await.unwrap();
    let bookmarks = &list.bookmarks;
    assert!(
        !bookmarks
            .iter()
            .any(|b| b.qualified_name.ends_with("/ephemeral")),
        "ephemeral bookmark should be dropped on anon disconnect; got: {bookmarks:?}"
    );
}
