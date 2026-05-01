//! Smoke tests for the in-process daemon harness defined in
//! `crates/core/src/test_support.rs`.
#![cfg(feature = "test-support")]

use logmon_broker_core::gelf::message::Level;
use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{LogsRecentResult, StatusGetResult};
use serde_json::json;

#[tokio::test]
async fn harness_starts_and_status_responds() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let result: StatusGetResult = client.call("status.get", json!({})).await.unwrap();
    // The harness disables real receivers, so the daemon advertises an empty
    // receivers list. uptime must be >= 0 (u64); just sanity-check the shape.
    assert!(result.receivers.is_empty(), "expected no receivers in test harness, got {:?}", result.receivers);
}

#[tokio::test]
async fn harness_inject_log_visible_to_recent() {
    let daemon = spawn_test_daemon().await;
    daemon.inject_log(Level::Error, "synthetic error").await;
    // Give the log_processor a tick to drain the channel and persist the entry.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut client = daemon.connect_anon().await;
    let result: LogsRecentResult = client
        .call("logs.recent", json!({ "count": 10 }))
        .await
        .unwrap();
    assert!(
        result.logs.iter().any(|e| e.message == "synthetic error"),
        "expected to see the injected log entry; got {:?}",
        result.logs.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}
