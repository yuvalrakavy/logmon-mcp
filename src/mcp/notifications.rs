use crate::engine::pipeline::PipelineEvent;
use rmcp::model::{CustomNotification, ServerNotification};
use rmcp::Peer;
use rmcp::RoleServer;
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// Method name used for trigger-fired MCP notifications.
pub const TRIGGER_FIRED_METHOD: &str = "notifications/trigger_fired";

/// Spawns a background task that receives `PipelineEvent`s from the broadcast
/// channel and forwards them to the connected MCP client as custom notifications.
///
/// The task runs until the broadcast sender is dropped (i.e., the pipeline is
/// shut down) or the MCP peer disconnects.
pub fn spawn_notification_dispatcher(
    mut rx: broadcast::Receiver<PipelineEvent>,
    peer: Peer<RoleServer>,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let payload = build_notification_payload(&event);
                    let notification = ServerNotification::CustomNotification(
                        CustomNotification::new(TRIGGER_FIRED_METHOD, Some(payload)),
                    );
                    if let Err(e) = peer.send_notification(notification).await {
                        warn!("Failed to send trigger notification to MCP client: {e}");
                        // Peer has disconnected; stop the dispatcher.
                        break;
                    }
                    debug!(
                        trigger_id = event.trigger_id,
                        "Sent trigger_fired notification to MCP client"
                    );
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Notification dispatcher lagged, skipped {n} pipeline events");
                    // Continue; don't crash on lag.
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // Pipeline dropped the sender; clean shutdown.
                    debug!("Pipeline event channel closed, notification dispatcher exiting");
                    break;
                }
            }
        }
    });
}

/// Serializes a `PipelineEvent` into the JSON payload expected by the spec.
fn build_notification_payload(event: &PipelineEvent) -> serde_json::Value {
    serde_json::json!({
        "trigger_id": event.trigger_id,
        "trigger_description": event.trigger_description,
        "filter": event.filter_string,
        "matched_entry": event.matched_entry,
        "context_before": event.context_before,
        "pre_trigger_flushed": event.pre_trigger_flushed,
        "post_window_size": event.post_window_size,
        "buffer_size": null, // populated by the caller if needed; left null here
    })
}
