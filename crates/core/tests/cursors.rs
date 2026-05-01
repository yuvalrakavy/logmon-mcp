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
