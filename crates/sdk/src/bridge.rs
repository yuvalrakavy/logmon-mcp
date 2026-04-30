use crate::transport;
use crate::Notification;
use logmon_broker_protocol::*;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, oneshot, Mutex, Notify};

/// Errors raised by the daemon bridge. Distinguishes transport errors,
/// remote RPC method errors, and bookkeeping failures so callers (and the
/// SDK's typed dispatch layer) can match without string parsing.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("transport: {0}")]
    Transport(#[from] std::io::Error),
    #[error("rpc error {code}: {message}")]
    Rpc { code: i32, message: String },
    #[error("response channel closed")]
    Closed,
    #[error("protocol: {0}")]
    Protocol(String),
}

/// Owns the writer half of the daemon socket and the pending-response map.
/// The reader half runs in a background task spawned by [`DaemonBridge::spawn`]
/// or [`DaemonBridge::start_reader`].
pub struct DaemonBridge {
    writer: Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>>,
    next_id: AtomicU64,
    notification_tx: broadcast::Sender<Notification>,
    /// Notified when the reader task exits (EOF or transport error). The
    /// reconnect machinery awaits this to detect connection loss.
    disconnect: Arc<Notify>,
}

/// Reader half of a not-yet-spawned bridge, used by the reconnect path so
/// that `Notification::Reconnected` can be broadcast BEFORE the reader task
/// starts converting daemon-drained `trigger_fired` notifications into
/// [`Notification::TriggerFired`] events.
///
/// The caller is responsible for invoking [`DaemonBridge::start_reader`]
/// once the ordering-critical work is done.
pub struct PendingReader {
    reader: Box<dyn AsyncBufRead + Unpin + Send>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>>,
    notification_tx: broadcast::Sender<Notification>,
    disconnect: Arc<Notify>,
}

impl PendingReader {
    /// Mutable access to the buffered reader. Used by the reconnect path to
    /// read the `session.start` response synchronously before the reader
    /// loop is spawned.
    pub fn reader_mut(&mut self) -> &mut (dyn AsyncBufRead + Unpin + Send) {
        &mut *self.reader
    }
}

impl DaemonBridge {
    /// Splits `stream` into reader/writer halves, spawns the reader loop, and
    /// returns a bridge that routes calls and forwards typed
    /// [`Notification`]s onto `notification_tx`. Unparseable notification
    /// frames are logged and dropped — subscribers only see well-formed
    /// events.
    ///
    /// The bridge owns an internal `Notify` that fires when the reader task
    /// exits (EOF or transport error); use [`Self::disconnect_notify`] to
    /// wait for that signal from the reconnect machinery.
    pub async fn spawn<S>(
        stream: S,
        notification_tx: broadcast::Sender<Notification>,
    ) -> Result<Self, BridgeError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (bridge, pending) = Self::spawn_without_reader(stream, notification_tx).await?;
        bridge.start_reader(pending);
        Ok(bridge)
    }

    /// Construct the bridge and split the stream, but DO NOT spawn the reader
    /// task yet. The caller must call [`Self::start_reader`] (or drop the
    /// returned [`PendingReader`]) before any RPC will receive a response.
    ///
    /// Used by the reconnect path so that ordering-sensitive work (broadcasting
    /// `Notification::Reconnected`) happens before the reader starts processing
    /// daemon-drained queue notifications.
    pub async fn spawn_without_reader<S>(
        stream: S,
        notification_tx: broadcast::Sender<Notification>,
    ) -> Result<(Self, PendingReader), BridgeError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let buf_reader = tokio::io::BufReader::new(reader);
        let disconnect = Arc::new(Notify::new());

        let bridge = Self {
            writer: Mutex::new(Box::new(writer)),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            notification_tx: notification_tx.clone(),
            disconnect: disconnect.clone(),
        };
        let pending_reader = PendingReader {
            reader: Box::new(buf_reader),
            pending,
            notification_tx,
            disconnect,
        };
        Ok((bridge, pending_reader))
    }

    /// Spawn the reader task using the [`PendingReader`] returned from
    /// [`Self::spawn_without_reader`]. After this returns, in-flight RPCs
    /// receive responses normally and notifications start flowing onto
    /// `notification_tx`.
    pub fn start_reader(&self, pending_reader: PendingReader) {
        let PendingReader {
            reader,
            pending,
            notification_tx,
            disconnect,
        } = pending_reader;
        // The bridge and the pending_reader share the same Arc<pending> map
        // (returned from spawn_without_reader), so the reader task can route
        // responses back to whoever is awaiting them via Self::call.
        debug_assert!(Arc::ptr_eq(&self.pending, &pending));
        spawn_reader_loop(reader, pending, notification_tx, disconnect);
    }

    /// Send an RPC request and wait for the response.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, BridgeError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = RpcRequest::new(id, method, params);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        {
            let mut writer = self.writer.lock().await;
            transport::write_message(&mut *writer, &request)
                .await
                .map_err(|e| BridgeError::Transport(io::Error::new(io::ErrorKind::Other, e)))?;
        }
        let response = rx.await.map_err(|_| BridgeError::Closed)?;
        match response.error {
            Some(err) => Err(BridgeError::Rpc {
                code: err.code,
                message: err.message,
            }),
            None => Ok(response.result.unwrap_or(serde_json::Value::Null)),
        }
    }

    /// Subscribe to daemon-originated typed notifications.
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Notification> {
        self.notification_tx.subscribe()
    }

    /// Returns the `Notify` that fires when the reader task exits (EOF or
    /// transport error). The reconnect machinery awaits this to drive the
    /// transition into `Reconnecting`.
    pub fn disconnect_notify(&self) -> Arc<Notify> {
        self.disconnect.clone()
    }

    /// Lock the writer half. Used by the reconnect path to write the
    /// `session.start` request synchronously without going through the
    /// usual id-based [`Self::call`] dispatch (the response is read off
    /// the [`PendingReader`] before the reader loop is spawned, so the
    /// pending-id map isn't consulted).
    pub async fn writer_lock(
        &self,
    ) -> tokio::sync::MutexGuard<'_, Box<dyn AsyncWrite + Unpin + Send>> {
        self.writer.lock().await
    }
}

/// Reader loop: routes responses to the pending-call map and converts wire
/// notifications into typed [`Notification`]s before broadcasting. Unparseable
/// or unknown frames are logged and dropped. On EOF or transport error, fires
/// the `disconnect` signal so the reconnect machinery can take over.
fn spawn_reader_loop(
    mut reader: Box<dyn AsyncBufRead + Unpin + Send>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>>,
    notification_tx: broadcast::Sender<Notification>,
    disconnect: Arc<Notify>,
) {
    tokio::spawn(async move {
        loop {
            match transport::read_daemon_message(&mut reader).await {
                Ok(Some(DaemonMessage::Response(resp))) => {
                    if let Some(tx) = pending.lock().await.remove(&resp.id) {
                        let _ = tx.send(resp);
                    }
                }
                Ok(Some(DaemonMessage::Notification(notif))) => {
                    if let Some(typed) = notification_from_wire(&notif) {
                        let _ = notification_tx.send(typed);
                    }
                }
                Ok(None) => {
                    tracing::debug!("bridge reader: EOF, signaling disconnect");
                    disconnect.notify_waiters();
                    break;
                }
                Err(e) => {
                    tracing::warn!("RPC read error: {e}");
                    disconnect.notify_waiters();
                    break;
                }
            }
        }
    });
}

/// Convert a wire [`RpcNotification`] into the SDK's typed [`Notification`].
/// Returns `None` for unknown methods or malformed payloads (logged at warn /
/// debug level so subscribers don't see noise).
fn notification_from_wire(notif: &RpcNotification) -> Option<Notification> {
    match notif.method.as_str() {
        // Underscore form is what the daemon currently emits
        // (`core::daemon::server::TRIGGER_FIRED_METHOD`); accept the dotted
        // form too for robustness against future renames.
        "trigger_fired" | "notifications/trigger_fired" | "trigger.fired" => {
            match serde_json::from_value::<TriggerFiredPayload>(notif.params.clone()) {
                Ok(payload) => Some(Notification::TriggerFired(payload)),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse trigger_fired payload");
                    None
                }
            }
        }
        other => {
            tracing::debug!(method = %other, "ignoring unknown notification");
            None
        }
    }
}
