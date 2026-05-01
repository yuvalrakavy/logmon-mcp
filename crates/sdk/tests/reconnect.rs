//! Integration tests for the SDK reconnect state machine (Task 16).
//!
//! Three scenarios:
//!
//! - **Named session resumes** across a daemon restart with state.json
//!   preserved → expect `Notification::Reconnected` and the trigger we added
//!   pre-restart still listed afterwards.
//! - **Anonymous session** has no resume identity → any disconnect transitions
//!   directly to `BrokerError::SessionLost`, no reconnect attempted.
//! - **Resurrection** — daemon comes back fresh (`is_new: true` on reconnect)
//!   → `BrokerError::SessionLost`, NO `Reconnected` event.

use std::sync::Once;
use std::time::Duration;

use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;
use logmon_broker_protocol::{TriggersAdd, TriggersList};
use logmon_broker_sdk::{Broker, BrokerError, Notification};

const TIGHT_TIMEOUT: Duration = Duration::from_secs(5);

static TRACING_INIT: Once = Once::new();
fn init_tracing() {
    TRACING_INIT.call_once(|| {
        // Best-effort: ignore if a subscriber is already installed.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .with_test_writer()
            .try_init();
    });
}

#[tokio::test]
async fn named_session_resumes_across_daemon_restart() {
    init_tracing();
    let mut daemon = spawn_test_daemon_for_sdk().await;

    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .session_name("resume_test")
        .call_timeout(TIGHT_TIMEOUT)
        .open()
        .await
        .expect("initial connect");

    let added = broker
        .triggers_add(TriggersAdd {
            filter: "l>=ERROR".into(),
            ..Default::default()
        })
        .await
        .expect("triggers_add");

    let mut sub = broker.subscribe_notifications();

    // Restart the daemon — preserves state.json so the named session can
    // resume.
    daemon.restart().await;

    // First notification on the channel must be Reconnected. Loop because
    // there may be unrelated default-trigger fires queued (we didn't inject
    // any logs, so this should be clean, but be defensive against future
    // daemon-side default-trigger changes).
    let deadline = tokio::time::Instant::now() + TIGHT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for Reconnected notification");
        }
        match tokio::time::timeout(remaining, sub.recv()).await {
            Ok(Ok(Notification::Reconnected)) => break,
            Ok(Ok(other)) => panic!("expected Reconnected first; got {other:?}"),
            Ok(Err(e)) => panic!("notification channel error: {e:?}"),
            Err(_) => panic!("timed out waiting for Reconnected notification"),
        }
    }

    // Trigger should still be present on the resumed session.
    let list = broker
        .triggers_list(TriggersList {})
        .await
        .expect("triggers_list after reconnect");
    assert!(
        list.triggers.iter().any(|t| t.id == added.id),
        "trigger {} should survive restart; got {:?}",
        added.id,
        list.triggers
    );
}

#[tokio::test]
async fn anonymous_session_reconnect_fails_with_session_lost() {
    init_tracing();
    let mut daemon = spawn_test_daemon_for_sdk().await;

    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        // No session_name → anonymous; cannot resume across restart.
        .call_timeout(TIGHT_TIMEOUT)
        .open()
        .await
        .expect("initial connect");

    daemon.restart().await;

    // Anonymous + disconnect should transition directly to SessionLost.
    // Subsequent calls return BrokerError::SessionLost without any reconnect
    // attempt.
    let result = broker.triggers_list(TriggersList {}).await;
    match result {
        Err(BrokerError::SessionLost) => {}
        other => panic!("expected SessionLost; got {other:?}"),
    }
}

#[tokio::test]
async fn resurrection_treated_as_session_lost() {
    init_tracing();
    let mut daemon = spawn_test_daemon_for_sdk().await;

    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .session_name("resurrect_test")
        .call_timeout(TIGHT_TIMEOUT)
        .open()
        .await
        .expect("initial connect");

    let mut sub = broker.subscribe_notifications();

    // Wipe state.json then restart — daemon comes back not knowing about us.
    daemon.wipe_state_and_restart().await;

    let result = broker.triggers_list(TriggersList {}).await;
    match result {
        Err(BrokerError::SessionLost) => {}
        other => panic!("expected SessionLost on resurrection; got {other:?}"),
    }

    // No Reconnected should fire on resurrection — verify the channel
    // doesn't deliver one within a short window. (If reconnect succeeded
    // prematurely we'd see a Reconnected before the SessionLost surfaces.)
    let saw_reconnected =
        tokio::time::timeout(Duration::from_millis(200), async {
            loop {
                match sub.recv().await {
                    Ok(Notification::Reconnected) => return true,
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false);
    assert!(
        !saw_reconnected,
        "Reconnected must not fire on daemon resurrection"
    );
}
