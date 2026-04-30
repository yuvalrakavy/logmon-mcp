use logmon_broker_sdk::Notification;
use rmcp::model::{CustomNotification, ServerNotification};
use rmcp::Peer;
use rmcp::RoleServer;
use tokio::sync::broadcast;
use tracing::warn;

/// Method name used for trigger-fired MCP notifications.
pub const TRIGGER_FIRED_METHOD: &str = "notifications/trigger_fired";

/// Spawns a background task that receives typed [`Notification`]s from the
/// SDK broadcast channel and forwards them to the connected MCP client as
/// custom notifications.
///
/// `Notification::TriggerFired` is re-serialized to its wire-shape JSON
/// payload so the MCP client sees the same params it always has.
/// `Notification::Reconnected` is intentionally not forwarded — there is no
/// MCP-level event for that today; subscribers that need it should consume
/// the typed channel directly.
///
/// The task runs until the broadcast sender is dropped (i.e., the daemon
/// connection closes) or the MCP peer disconnects.
pub fn spawn_notification_forwarder(
    mut rx: broadcast::Receiver<Notification>,
    peer: Peer<RoleServer>,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(Notification::TriggerFired(payload)) => {
                    let params = match serde_json::to_value(&payload) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("Failed to serialize trigger_fired payload: {e}");
                            continue;
                        }
                    };
                    let notification = ServerNotification::CustomNotification(
                        CustomNotification::new(TRIGGER_FIRED_METHOD, Some(params)),
                    );
                    if let Err(e) = peer.send_notification(notification).await {
                        warn!("Failed to send notification to MCP client: {e}");
                        break;
                    }
                }
                Ok(Notification::Reconnected) => {
                    // No MCP-level mapping yet; ignore.
                }
                Ok(_) => {
                    // `Notification` is `#[non_exhaustive]`; unknown future
                    // variants land here.
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Notification forwarder lagged, skipped {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
