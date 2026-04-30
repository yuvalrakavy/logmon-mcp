//! Integration tests for the `oneshot` trigger flag.
//!
//! Verifies that:
//! 1. A trigger created with `oneshot: true` fires once and is then
//!    auto-removed (no second notification).
//! 2. A trigger created without `oneshot` (defaults to false) fires
//!    repeatedly.
//! 3. Persisted oneshot triggers survive a daemon restart with their
//!    `oneshot` flag preserved.
#![cfg(feature = "test-support")]

use logmon_broker_core::daemon::persistence::{
    DaemonState, PersistedSession, PersistedTrigger,
};
use logmon_broker_core::gelf::message::Level;
use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{
    TriggersAddResult, TriggersListResult,
};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn oneshot_trigger_removes_after_first_match() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // Drop the default triggers so the assertion "no notification after
    // oneshot removal" actually exercises the oneshot path rather than the
    // default ERROR trigger's post-window suppressing the second log.
    let initial: TriggersListResult = client.call("triggers.list", json!({})).await.unwrap();
    for t in &initial.triggers {
        let _: serde_json::Value = client
            .call("triggers.remove", json!({ "id": t.id }))
            .await
            .unwrap();
    }

    // Add a oneshot trigger matching ERROR-level logs.
    let resp: TriggersAddResult = client
        .call(
            "triggers.add",
            json!({
                "filter": "l>=ERROR",
                "oneshot": true,
                "description": "oneshot test"
            }),
        )
        .await
        .unwrap();
    let trigger_id = resp.id;

    // Inject one ERROR log → trigger fires once.
    daemon.inject_log(Level::Error, "first error").await;
    let notif = client.expect_trigger_fired(trigger_id).await;
    // The wire payload must report oneshot=true so the client knows this
    // is the one and only fire.
    assert_eq!(
        notif.params.get("oneshot").and_then(|v| v.as_bool()),
        Some(true),
        "expected oneshot=true on TriggerFiredPayload, got: {:?}",
        notif.params
    );

    // Verify trigger removed via triggers.list (give the processor a tick to
    // run the post-dispatch removal).
    tokio::time::sleep(Duration::from_millis(50)).await;
    let list: TriggersListResult = client.call("triggers.list", json!({})).await.unwrap();
    assert!(
        !list.triggers.iter().any(|t| t.id == trigger_id),
        "oneshot trigger {} should be auto-removed after firing; got {:?}",
        trigger_id,
        list.triggers.iter().map(|t| t.id).collect::<Vec<_>>()
    );

    // Inject another ERROR — should NOT fire again (the trigger is gone).
    daemon.inject_log(Level::Error, "second error").await;
    let no_notif = client.try_recv_notification(Duration::from_millis(200)).await;
    assert!(
        no_notif.is_none(),
        "expected no notification after oneshot removal, got: {no_notif:?}"
    );
}

#[tokio::test]
async fn non_oneshot_trigger_persists() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // Remove the default triggers so the session's post-window is driven
    // solely by our test trigger (post_window=0 → no post-window suppression
    // after fire). Otherwise the default ERROR trigger's post_window=200
    // swallows our second log, masking the actual fire-twice behavior.
    let initial: TriggersListResult = client.call("triggers.list", json!({})).await.unwrap();
    for t in &initial.triggers {
        let _: serde_json::Value = client
            .call("triggers.remove", json!({ "id": t.id }))
            .await
            .unwrap();
    }

    // Add a regular trigger (oneshot defaults to false).
    let resp: TriggersAddResult = client
        .call(
            "triggers.add",
            json!({
                "filter": "l>=ERROR",
                "description": "regular ERROR watch"
            }),
        )
        .await
        .unwrap();
    let trigger_id = resp.id;

    // Verify list reports oneshot=false explicitly.
    let list: TriggersListResult = client.call("triggers.list", json!({})).await.unwrap();
    let info = list
        .triggers
        .iter()
        .find(|t| t.id == trigger_id)
        .expect("trigger missing from list");
    assert!(
        !info.oneshot,
        "expected oneshot=false by default on triggers.list, got {info:?}"
    );

    daemon.inject_log(Level::Error, "first error").await;
    let notif = client.expect_trigger_fired(trigger_id).await;
    assert_eq!(
        notif.params.get("oneshot").and_then(|v| v.as_bool()),
        Some(false),
        "non-oneshot trigger should report oneshot=false on the wire"
    );

    // Trigger should still be there.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let list: TriggersListResult = client.call("triggers.list", json!({})).await.unwrap();
    assert!(
        list.triggers
            .iter()
            .any(|t| t.id == trigger_id && !t.oneshot),
        "non-oneshot trigger {} should persist after firing; got {:?}",
        trigger_id,
        list.triggers
    );

    // Second log fires it again.
    daemon.inject_log(Level::Error, "second error").await;
    let _ = client.expect_trigger_fired(trigger_id).await;
}

#[tokio::test]
async fn restored_trigger_preserves_oneshot() {
    // Pre-populate state.json so the daemon restores a named session with a
    // oneshot trigger at startup. Then connect and verify triggers.list
    // surfaces oneshot=true.
    let tempdir = std::sync::Arc::new(tempfile::TempDir::new().unwrap());
    let state_path = tempdir.path().join("state.json");
    let mut state = DaemonState::default();
    state.named_sessions.insert(
        "persisted".to_string(),
        PersistedSession {
            triggers: vec![PersistedTrigger {
                filter: "l>=ERROR".to_string(),
                pre_window: 0,
                post_window: 0,
                notify_context: 0,
                description: Some("restored oneshot".to_string()),
                oneshot: true,
            }],
            filters: vec![],
        },
    );
    let json = serde_json::to_string_pretty(&state).unwrap();
    std::fs::write(&state_path, json).unwrap();

    let daemon = TestDaemonHandle::spawn_in_tempdir(tempdir).await;
    let mut client = daemon.connect_named("persisted", None).await;

    let list: TriggersListResult = client.call("triggers.list", json!({})).await.unwrap();
    let restored = list
        .triggers
        .iter()
        .find(|t| t.filter == "l>=ERROR" && t.description.as_deref() == Some("restored oneshot"))
        .expect("restored oneshot trigger missing from list");
    assert!(
        restored.oneshot,
        "restored trigger must preserve oneshot=true across daemon restart, got {restored:?}"
    );
}
