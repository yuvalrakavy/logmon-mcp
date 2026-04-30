use crate::rpc::types::RpcNotification;
use rmcp::model::{CustomNotification, ServerNotification};
use rmcp::Peer;
use rmcp::RoleServer;
use tokio::sync::broadcast;
use tracing::warn;

/// Method name used for trigger-fired MCP notifications.
pub const TRIGGER_FIRED_METHOD: &str = "notifications/trigger_fired";

/// Spawns a background task that receives `RpcNotification`s from the daemon
/// broadcast channel and forwards them to the connected MCP client as custom
/// notifications.
///
/// The task runs until the broadcast sender is dropped (i.e., the daemon
/// connection closes) or the MCP peer disconnects.
pub fn spawn_notification_forwarder(
    mut rx: broadcast::Receiver<RpcNotification>,
    peer: Peer<RoleServer>,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(notif) => {
                    let notification = ServerNotification::CustomNotification(
                        CustomNotification::new(TRIGGER_FIRED_METHOD, Some(notif.params)),
                    );
                    if let Err(e) = peer.send_notification(notification).await {
                        warn!("Failed to send notification to MCP client: {e}");
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Notification forwarder lagged, skipped {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
