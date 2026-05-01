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
