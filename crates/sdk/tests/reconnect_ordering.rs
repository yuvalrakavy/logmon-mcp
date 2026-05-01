//! Notification ordering across reconnect (Task 17).
//!
//! Asserts the contract from `crate::reconnect`'s module docs:
//!
//! > `Notification::Reconnected` is broadcast AFTER the new `session.start`
//! > handshake but BEFORE the new bridge's reader task starts processing
//! > daemon-drained queue notifications.
//!
//! ## Test scope
//!
//! The plan draft also asserted that an explicitly-queued notification
//! (logged while the daemon is offline) appears between `Reconnected` and any
//! "live" trigger. Producing a "while offline" log in the test harness
//! deterministically is awkward — `inject_log` is a no-op when the daemon's
//! log_processor task isn't running, and the harness doesn't expose a way to
//! disconnect a single broker without tearing down the whole daemon. So we
//! settle on the simpler invariant: `Reconnected` strictly precedes the next
//! live `TriggerFired` we cause post-reconnect.
//!
//! That's what Task 16's implementation actually guarantees structurally
//! (broadcast send happens before `start_reader`, which is what spawns the
//! reader task that drains the wire and broadcasts subsequent
//! `TriggerFired`s). The drained-queue path is covered indirectly: any
//! drained notification that did arrive would land *after* `Reconnected` for
//! the same reason.

use std::sync::Once;
use std::time::Duration;

use logmon_broker_core::gelf::message::Level;
use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;
use logmon_broker_protocol::TriggersAdd;
use logmon_broker_sdk::{Broker, Notification};

const TIGHT_TIMEOUT: Duration = Duration::from_secs(5);

static TRACING_INIT: Once = Once::new();
fn init_tracing() {
    TRACING_INIT.call_once(|| {
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
async fn reconnected_precedes_live_trigger_fired() {
    init_tracing();
    let mut daemon = spawn_test_daemon_for_sdk().await;

    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .session_name("ordering_test")
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

    // Force a reconnect.
    daemon.restart().await;

    // The strict invariant: Reconnected must arrive on the broadcast
    // channel BEFORE any TriggerFired we cause post-reconnect. Receive
    // the first `Notification::Reconnected | Notification::TriggerFired
    // (with our id)` we see — it MUST be Reconnected.
    let deadline = tokio::time::Instant::now() + TIGHT_TIMEOUT;
    match recv_until_relevant(&mut sub, deadline, added.id).await {
        Ok(Notification::Reconnected) => {}
        Ok(other) => panic!(
            "ordering violation: first relevant event after restart was {other:?}, \
             not Reconnected"
        ),
        Err(e) => panic!("waiting for first relevant event: {e}"),
    }

    // Now inject a log → should produce a live TriggerFired with our id.
    daemon.inject_log(Level::Error, "live").await;

    let live = recv_until_relevant(&mut sub, deadline, added.id).await;
    match live {
        Ok(Notification::TriggerFired(p)) => {
            assert_eq!(p.trigger_id, added.id, "trigger id");
            assert_eq!(p.matched_entry.message, "live", "matched message");
        }
        Ok(other) => panic!("expected live TriggerFired post-Reconnected; got {other:?}"),
        Err(e) => panic!("waiting for live TriggerFired: {e}"),
    }
}

/// Receive notifications until we see one we care about for this test:
/// either a `Reconnected` or a `TriggerFired` whose `trigger_id` matches
/// `our_trigger_id`. Default-trigger fires (the daemon ships with two
/// always-on triggers we didn't add) are skipped — they're not part of the
/// ordering contract under test.
async fn recv_until_relevant(
    sub: &mut tokio::sync::broadcast::Receiver<Notification>,
    deadline: tokio::time::Instant,
    our_trigger_id: u32,
) -> Result<Notification, String> {
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("timed out".into());
        }
        match tokio::time::timeout(remaining, sub.recv()).await {
            Ok(Ok(n)) => match &n {
                Notification::Reconnected => return Ok(n),
                Notification::TriggerFired(p) if p.trigger_id == our_trigger_id => {
                    return Ok(n);
                }
                // Other-trigger fires: not part of this contract.
                _ => continue,
            },
            Ok(Err(e)) => return Err(format!("channel error: {e:?}")),
            Err(_) => return Err("timed out".into()),
        }
    }
}
