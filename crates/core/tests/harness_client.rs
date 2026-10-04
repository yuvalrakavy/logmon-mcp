//! The harness's own client fails a call when its connection is gone, instead of waiting for a
//! reply that cannot come. A control run once sat 9 minutes at 0% CPU on such a call; a test
//! that hangs reports nothing.
#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The case that hung: a call already WAITING when the connection goes. A fake daemon answers
/// the handshake, reads the next request, and hangs up without replying — deterministic, unlike
/// racing a real daemon's shutdown against a call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_in_flight_when_the_connection_closes_fails_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("fake.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let fake = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = tokio::io::split(stream);
        let mut lines = BufReader::new(r).lines();
        let start: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let reply = json!({
            "jsonrpc": "2.0",
            "id": start["id"],
            "result": {
                "session_id": "fake", "is_new": true, "queued_notifications": 0,
                "trigger_count": 0, "filter_count": 0, "daemon_uptime_secs": 0,
                "buffer_size": 0, "receivers": [], "capabilities": []
            }
        });
        w.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
        // Read the call, then hang up without answering it.
        let _call = lines.next_line().await.unwrap();
    });
    let mut c = TestClient::try_connect(&sock, None, None, None)
        .await
        .expect("handshake with the fake daemon");

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        c.call::<Value>("status.get", json!({})),
    )
    .await
    .expect("the call returned instead of waiting for a reply that cannot come");
    let err = outcome.expect_err("a call whose connection closed fails");
    assert!(
        err.to_string().contains("status.get") && err.to_string().contains("closed"),
        "the error names the call and why: {err}"
    );
    fake.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_on_a_closed_connection_fails_promptly_and_says_why() {
    let daemon = spawn_test_daemon().await;
    let mut c = daemon.connect_anon().await;
    daemon.shutdown().await;

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        c.call::<Value>("status.get", json!({})),
    )
    .await
    .expect("the call returned instead of waiting for a reply that cannot come");
    let err = outcome.expect_err("a call on a closed connection fails");
    assert!(
        err.to_string().contains("status.get"),
        "the error names the call: {err}"
    );
}
