//! Integration test for the SDK's typed Notification enum (Task 14).

use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;
use logmon_broker_protocol::TriggersAdd;
use logmon_broker_sdk::{Broker, Notification};

use logmon_broker_core::gelf::message::Level;

#[tokio::test]
async fn typed_trigger_fired_received() {
    let daemon = spawn_test_daemon_for_sdk().await;
    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .open()
        .await
        .unwrap();

    let mut sub = broker.subscribe_notifications();

    let added = broker
        .triggers_add(TriggersAdd {
            filter: "l>=ERROR".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    daemon.inject_log(Level::Error, "fire it").await;

    // The daemon ships with two default triggers (`l>=ERROR` and a panic
    // regex) that also match this log entry. Walk the channel until we see
    // the fire belonging to our just-added trigger; bail loudly on
    // unexpected channel states.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "timed out waiting for TriggerFired for trigger {}",
                added.id
            );
        }
        match tokio::time::timeout(remaining, sub.recv()).await {
            Ok(Ok(Notification::TriggerFired(payload))) if payload.trigger_id == added.id => {
                assert_eq!(payload.matched_entry.message, "fire it");
                assert_eq!(payload.filter_string, "l>=ERROR");
                return;
            }
            Ok(Ok(Notification::TriggerFired(_))) => continue,
            Ok(Ok(other)) => panic!("expected TriggerFired, got: {other:?}"),
            Ok(Err(e)) => panic!("notification channel error: {e:?}"),
            Err(_) => panic!(
                "timed out waiting for TriggerFired for trigger {}",
                added.id
            ),
        }
    }
}
