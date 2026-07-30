//! Trigger-window defaults (Wave 2 stage 2.3d, §6 decision #4). An ad-hoc
//! `triggers.add` that omits its windows now defaults to 500/200/5 — capturing
//! context by default — instead of the old 0/0/0. Explicit values (including an
//! explicit 0) are still honored.
#![cfg(feature = "test-support")]

use logmon_broker_core::gelf::message::Level;
use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{TriggersAddResult, TriggersEditResult, TriggersListResult};
use serde_json::json;

/// Poll `triggers.list` until `id` reports `want` matches. Ingest is async
/// (inject_log hands off to the processor task), so a bare sleep would be
/// either flaky or slow; the deadline bounds only the failure path.
async fn wait_for_match_count(client: &mut TestClient, id: u32, want: u64) {
    for _ in 0..200 {
        let list: TriggersListResult = client.call("triggers.list", json!({})).await.unwrap();
        if let Some(t) = list.triggers.iter().find(|t| t.id == id) {
            if t.match_count >= want {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("trigger {id} never reached match_count {want}");
}

/// REGRESSION: the daemon's `triggers.edit` response must survive
/// deserialization into the TYPED result struct.
///
/// `TriggersEditResult` is a separate struct from `TriggerInfo`, so a field
/// added to `TriggerInfo` does not appear on it. `post_remaining` was emitted
/// by the daemon while the typed struct lacked it, and serde silently dropped
/// it for every SDK caller. `cargo xtask verify-schema` cannot catch this
/// class — it compares the schema against the Rust types, not the daemon's
/// actual JSON against those types. Only a round-trip through the wire does.
#[tokio::test]
async fn triggers_edit_response_round_trips_through_the_typed_struct() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let add: TriggersAddResult = client
        .call(
            "triggers.add",
            json!({ "filter": "m=roundtrip-canary", "pre_window": 0, "post_window": 7 }),
        )
        .await
        .unwrap();

    // FIRE it, so post_remaining is NON-ZERO. This is the whole point: serde
    // ignores unknown fields, so if the typed struct lacked `post_remaining`
    // the value would silently default to 0 — indistinguishable from a live,
    // armed trigger. Only a non-zero expected value can tell the two apart.
    daemon.inject_log(Level::Info, "roundtrip-canary").await;
    wait_for_match_count(&mut client, add.id, 1).await;

    // Widen the window (7 < 42, so the clamp does not apply and the in-flight
    // remaining is preserved).
    let edited: TriggersEditResult = client
        .call("triggers.edit", json!({ "id": add.id, "post_window": 42 }))
        .await
        .unwrap();

    assert_eq!(edited.id, add.id);
    assert_eq!(edited.post_window, 42, "the edit applied");
    assert_eq!(
        edited.post_remaining, 7,
        "post_remaining must survive into the TYPED edit response; 0 here means \
         the field is missing from TriggersEditResult and serde defaulted it"
    );
}

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
    assert_eq!(
        t.pre_window, 0,
        "explicit 0 is honored, not replaced by the default"
    );
    assert_eq!(t.post_window, 10);
    assert_eq!(t.notify_context, 3);
}
