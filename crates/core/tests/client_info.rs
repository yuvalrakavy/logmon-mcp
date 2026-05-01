//! Integration tests for the `client_info` field on `session.start`.
//!
//! Verifies that:
//! 1. A caller-supplied `client_info` blob round-trips through `session.list`.
//! 2. Payloads exceeding 4 KB serialized are rejected with `-32602`.
//! 3. `client_info` for a named session survives a daemon restart.
#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::SessionListResult;
use serde_json::json;

#[tokio::test]
async fn client_info_round_trip() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon
        .connect_named(
            "test_session",
            Some(json!({
                "name": "store-test",
                "version": "0.1.0",
                "run_id": "abc"
            })),
        )
        .await;

    let list: SessionListResult = client.call("session.list", json!({})).await.unwrap();
    let session = list
        .sessions
        .iter()
        .find(|s| s.name.as_deref() == Some("test_session"))
        .expect("test_session should be listed");
    let ci = session.client_info.as_ref().expect("client_info present");
    assert_eq!(ci["name"], "store-test");
    assert_eq!(ci["version"], "0.1.0");
    assert_eq!(ci["run_id"], "abc");
}

#[tokio::test]
async fn client_info_size_limit() {
    let daemon = spawn_test_daemon().await;
    // Build a payload > 4 KB serialized.
    let huge = "x".repeat(5000);
    let result = daemon
        .try_connect_named("oversize", Some(json!({ "name": huge })))
        .await;
    let err = match result {
        Ok(_) => panic!("expected error for oversize client_info, got Ok"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("client_info exceeds 4 KB limit"),
        "got: {err}"
    );
}

#[tokio::test]
async fn client_info_persists_across_restart() {
    let mut daemon = spawn_test_daemon().await;
    {
        // Create the session with client_info, then drop the client to
        // disconnect. The named session stays in the registry (disconnected).
        let _client = daemon
            .connect_named(
                "persist_test",
                Some(json!({
                    "name": "store-test",
                    "run_id": "xyz"
                })),
            )
            .await;
        // _client is dropped here — disconnects the session.
    }

    // Restart the daemon. shutdown() is invoked by restart() and saves the
    // live snapshot to state.json, then a fresh daemon loads it.
    daemon.restart().await;

    let mut client = daemon.connect_named("persist_test", None).await;
    let list: SessionListResult = client.call("session.list", json!({})).await.unwrap();
    let session = list
        .sessions
        .iter()
        .find(|s| s.name.as_deref() == Some("persist_test"))
        .expect("persist_test should still exist after restart");
    let ci = session
        .client_info
        .as_ref()
        .expect("client_info should persist across restart");
    assert_eq!(ci["name"], "store-test");
    assert_eq!(ci["run_id"], "xyz");
}
