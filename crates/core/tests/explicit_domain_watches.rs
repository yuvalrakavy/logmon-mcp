#![cfg(feature = "test-support")]
use logmon_broker_core::gelf::message::Level;
use logmon_broker_core::test_support::*;
use serde_json::{json, Value};

#[tokio::test]
async fn pinned_watch_reaches_its_caller_across_domains_and_scopes() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let _: Value = client
        .call("domains.create", json!({"name":"lane"}))
        .await
        .unwrap();
    let _: Value = client
        .call("domains.use", json!({"name":"lane"}))
        .await
        .unwrap();
    let trigger: Value = client
        .call(
            "triggers.add",
            json!({"domain":"default","scope":"caller","filter":"l=INFO","oneshot":true,"pre_window":0,"post_window":0}),
        )
        .await
        .unwrap();
    let _: Value = client
        .call(
            "filters.add",
            json!({"domain":"default","scope":"caller","filter":"l=INFO"}),
        )
        .await
        .unwrap();
    let other: Value = client
        .call("filters.list", json!({"domain":"default","scope":"other"}))
        .await
        .unwrap();
    assert!(
        other["filters"].as_array().unwrap().is_empty(),
        "other scope saw filter: {other}"
    );
    let own: Value = client
        .call("filters.list", json!({"domain":"default","scope":"caller"}))
        .await
        .unwrap();
    assert_eq!(own["filters"].as_array().unwrap().len(), 1);
    daemon.inject_log(Level::Warn, "filtered out").await;
    daemon.inject_log(Level::Info, "pinned record").await;
    let notice = client
        .expect_trigger_fired(trigger["id"].as_u64().unwrap() as u32)
        .await;
    assert_eq!(notice.params["domain"], "default");
    assert_eq!(notice.params["scope"], "caller");
    assert_eq!(notice.params["matched_entry"]["message"], "pinned record");
    let stored: Value = client
        .call("logs.recent", json!({"domain":"default"}))
        .await
        .unwrap();
    let entries = stored["logs"].as_array().unwrap();
    assert!(entries.iter().any(|log| log["message"] == "pinned record"));
    assert!(entries.iter().all(|log| log["message"] != "filtered out"));
    let left: Value = client
        .call(
            "triggers.list",
            json!({"domain":"default","scope":"caller"}),
        )
        .await
        .unwrap();
    assert!(left["triggers"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn named_pins_persist_and_deliver_queued_notifications_after_reconnect() {
    let mut daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_named("monitor", None).await;
    let params = json!({"scope":"night","domain":"default","filter":"l=INFO","post_window":0});
    let _: Value = client.call("triggers.add", params.clone()).await.unwrap();
    let _: Value = client
        .call(
            "filters.add",
            json!({"scope":"night","domain":"default","filter":"l=INFO"}),
        )
        .await
        .unwrap();
    client.close().await.unwrap();
    daemon.restart().await;
    // Named watches evaluate while their owner is disconnected. Rebinding when
    // reconnecting must preserve queued pinned notifications.
    let mut observer = daemon.connect_anon().await;
    let _: Value = observer
        .call("domains.create", json!({"name":"lane"}))
        .await
        .unwrap();
    daemon.inject_log(Level::Info, "queued after restart").await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let sessions: Value = observer.call("sessions.list", json!({})).await.unwrap();
        if sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "monitor" && s["queue_size"].as_u64().unwrap_or(0) > 0)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "queued event missing: {sessions}"
        );
        tokio::task::yield_now().await;
    }
    let mut client = daemon.connect_with_domain(Some("monitor"), "lane").await;
    let own: Value = client
        .call("triggers.list", json!({"domain":"default","scope":"night"}))
        .await
        .unwrap();
    let tid = own["triggers"][0]["id"].as_u64().unwrap() as u32;
    let notice = client.expect_trigger_fired(tid).await;
    assert_eq!(notice.params["scope"], "night");
    assert_eq!(notice.params["domain"], "default");
    assert_eq!(
        notice.params["matched_entry"]["message"],
        "queued after restart"
    );
    let filters: Value = client
        .call("filters.list", json!({"domain":"default","scope":"night"}))
        .await
        .unwrap();
    assert_eq!(filters["filters"].as_array().unwrap().len(), 1);
}
