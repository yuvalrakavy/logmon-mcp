//! Trigger-window defaults (Wave 2 stage 2.3d, §6 decision #4). An ad-hoc
//! `triggers.add` that omits its windows now defaults to 500/200/5 — capturing
//! context by default — instead of the old 0/0/0. Explicit values (including an
//! explicit 0) are still honored.
#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{TriggersAddResult, TriggersListResult};
use serde_json::json;

#[tokio::test]
async fn triggers_add_defaults_windows_to_capture_context() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let add: TriggersAddResult = client
        .call("triggers.add", json!({ "filter": "fa=test" }))
        .await
        .unwrap();
    let list: TriggersListResult = client.call("triggers.list", json!({})).await.unwrap();
    let t = list
        .triggers
        .iter()
        .find(|t| t.id == add.id)
        .expect("added trigger present");
    assert_eq!(t.pre_window, 500, "omitted pre_window defaults to 500");
    assert_eq!(t.post_window, 200, "omitted post_window defaults to 200");
    assert_eq!(t.notify_context, 5, "omitted notify_context defaults to 5");
}

#[tokio::test]
async fn triggers_add_honors_explicit_windows_including_zero() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let add: TriggersAddResult = client
        .call(
            "triggers.add",
            json!({ "filter": "fa=test", "pre_window": 0, "post_window": 10, "notify_context": 3 }),
        )
        .await
        .unwrap();
    let list: TriggersListResult = client.call("triggers.list", json!({})).await.unwrap();
    let t = list.triggers.iter().find(|t| t.id == add.id).unwrap();
    assert_eq!(t.pre_window, 0, "explicit 0 is honored, not replaced by the default");
    assert_eq!(t.post_window, 10);
    assert_eq!(t.notify_context, 3);
}
