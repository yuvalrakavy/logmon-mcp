use crate::transport;
use crate::Notification;
use logmon_broker_protocol::*;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex};
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

/// The calls awaiting a reply on one connection, and whether that connection is gone.
///
/// One lock for both, so a call cannot register after the reader has given up: when the reader
/// stops (the daemon closed the connection — it restarted, or the request's handler panicked),
/// it marks the connection closed and drops every waiting sender, failing those calls with
/// [`BridgeError::Closed`]. Leaving them in the map hung each one forever — the call holds the
/// bridge, the bridge holds the map, the map holds the sender — even after a reconnect had
/// installed a new bridge.
#[derive(Default)]
struct Pending {
    calls: HashMap<u64, oneshot::Sender<RpcResponse>>,
    closed: bool,
}

/// A cancelled caller no longer needs a response slot. This synchronous mutex
/// only guards the ID map; no I/O happens under it.
struct ResponseSlot<'a> {
    id: u64,
    pending: &'a SyncMutex<Pending>,
}

impl Drop for ResponseSlot<'_> {
    fn drop(&mut self) {
        self.pending.lock().unwrap().calls.remove(&self.id);
    }
}

/// Owns the writer half of the daemon socket and the pending-response map.
/// The reader half runs in a background task spawned by [`DaemonBridge::spawn`]
/// or [`DaemonBridge::start_reader`].
pub struct DaemonBridge {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    pending: Arc<SyncMutex<Pending>>,
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
    pending: Arc<SyncMutex<Pending>>,
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
        let pending = Arc::new(SyncMutex::new(Pending::default()));
        let buf_reader = tokio::io::BufReader::new(reader);
        let disconnect = Arc::new(Notify::new());

        let bridge = Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
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
        self.call_inner(method, params, false).await
    }

    /// The same, asking the daemon for a rendered `_display` string beside the
    /// result.
    ///
    /// **This is the seam, not `RpcRequest::new`.** Neither surface that wants
    /// rendering constructs a request: the MCP route and the CLI both call
    /// `Broker::call`, which lands here. Of the constructor's call sites only
    /// this one carries a tool call; the rest are tests and `session.start`.
    ///
    /// A daemon with no renderer for `method` returns no `_display`, and the
    /// caller falls back to JSON — the same path as an old broker that does not
    /// know the flag at all.
    pub async fn call_rendered(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, BridgeError> {
        self.call_inner(method, params, true).await
    }

    async fn call_inner(
        &self,
        method: &str,
        params: serde_json::Value,
        display: bool,
    ) -> Result<serde_json::Value, BridgeError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = RpcRequest::new(id, method, params);
        let request = if display { request.rendered() } else { request };
        // Wait without queuing a detached task: at most one write can outlive
        // its caller if a socket stalls or a lifecycle deadline expires.
        let mut writer = self.writer.clone().lock_owned().await;
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.closed {
                return Err(BridgeError::Closed);
            }
            pending.calls.insert(id, tx);
        }
        let _slot = ResponseSlot {
            id,
            pending: &self.pending,
        };
        // A deadline can expire in the middle of a multi-write JSON frame.
        // Finish that frame before releasing the shared connection for reuse.
        tokio::spawn(async move { transport::write_message(&mut *writer, &request).await })
            .await
            .map_err(|e| BridgeError::Transport(io::Error::other(e)))?
            .map_err(|e| BridgeError::Transport(io::Error::other(e)))?;
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
    pending: Arc<SyncMutex<Pending>>,
    notification_tx: broadcast::Sender<Notification>,
    disconnect: Arc<Notify>,
) {
    tokio::spawn(async move {
        loop {
            match transport::read_daemon_message(&mut reader).await {
                Ok(Some(DaemonMessage::Response(resp))) => {
                    if let Some(tx) = pending.lock().unwrap().calls.remove(&resp.id) {
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
                    break;
                }
                Err(e) => {
                    tracing::warn!("RPC read error: {e}");
                    break;
                }
            }
        }
        // No reply can arrive any more: fail every call still waiting, and refuse new ones —
        // see `Pending`.
        {
            let mut pending = pending.lock().unwrap();
            pending.closed = true;
            pending.calls.clear();
        }
        disconnect.notify_waiters();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncBufReadExt;

    #[tokio::test]
    async fn cancelled_call_releases_its_slot_and_finishes_its_frame() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (client, daemon) = tokio::io::duplex(32);
        let (tx, _) = broadcast::channel(4);
        let bridge = Arc::new(DaemonBridge::spawn(client, tx).await.unwrap());
        let calling = bridge.clone();
        let first = tokio::spawn(async move {
            calling
                .call("test_log.end", serde_json::json!({"data": "x".repeat(512)}))
                .await
        });
        let mut daemon = tokio::io::BufReader::new(daemon);
        let mut prefix = [0; 1];
        daemon.read_exact(&mut prefix).await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert!(bridge.pending.lock().unwrap().calls.is_empty());

        let server = tokio::spawn(async move {
            let mut first_frame = String::from_utf8(prefix.to_vec()).unwrap();
            daemon.read_line(&mut first_frame).await.unwrap();
            let req: RpcRequest = serde_json::from_str(&first_frame).unwrap();
            assert_eq!(req.method, "test_log.end");
            let next = transport::read_request(&mut daemon).await.unwrap().unwrap();
            let reply = serde_json::json!({"jsonrpc":"2.0","id":next.id,"result":"ok"});
            daemon
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        });
        let next = tokio::time::timeout(
            Duration::from_secs(3),
            bridge.call("status.get", serde_json::json!({})),
        )
        .await
        .expect("subsequent request can use the connection")
        .unwrap();
        assert_eq!(next, "ok");
        server.await.unwrap();
    }

    /// A daemon that closes the connection without answering — it restarted, or the request's
    /// handler panicked — fails the call in flight, and every later call on that connection,
    /// instead of leaving them waiting forever.
    #[tokio::test]
    async fn a_connection_closed_mid_call_fails_the_call_instead_of_hanging() {
        let (client, daemon) = tokio::net::UnixStream::pair().unwrap();
        let (tx, _rx) = broadcast::channel(4);
        let bridge = DaemonBridge::spawn(client, tx).await.unwrap();

        // The daemon reads the request and closes the connection without a reply.
        let daemon = tokio::spawn(async move {
            let mut daemon = tokio::io::BufReader::new(daemon);
            let mut line = String::new();
            daemon.read_line(&mut line).await.unwrap();
            assert!(line.contains("status.get"), "{line}");
        });

        let call = tokio::time::timeout(
            Duration::from_secs(10),
            bridge.call("status.get", serde_json::json!({})),
        )
        .await
        .expect("the call ends when its connection closes");
        assert!(matches!(call, Err(BridgeError::Closed)), "{call:?}");
        daemon.await.unwrap();

        let later = tokio::time::timeout(
            Duration::from_secs(10),
            bridge.call("status.get", serde_json::json!({})),
        )
        .await
        .expect("a call on a closed connection ends at once");
        assert!(later.is_err(), "{later:?}");
    }

    /// A reader that stops on a frame it cannot parse leaves the socket WRITABLE, so a later
    /// call's request goes out without error — and with no reader, no reply could ever reach
    /// it. It is refused instead.
    #[tokio::test]
    async fn a_call_after_the_reader_stopped_is_refused_though_the_socket_is_open() {
        let (client, daemon) = tokio::net::UnixStream::pair().unwrap();
        let (tx, _rx) = broadcast::channel(4);
        let bridge = DaemonBridge::spawn(client, tx).await.unwrap();

        // The daemon answers garbage and keeps the connection open, reading on.
        let (daemon_reader, mut daemon_writer) = tokio::io::split(daemon);
        tokio::io::AsyncWriteExt::write_all(&mut daemon_writer, b"not json\n")
            .await
            .unwrap();
        let drain = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(daemon_reader).lines();
            while let Ok(Some(_)) = lines.next_line().await {}
            drop(daemon_writer);
        });

        // Wait until the reader has given up on the garbage.
        for _ in 0..500 {
            if bridge.pending.lock().unwrap().closed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(bridge.pending.lock().unwrap().closed, "the reader stopped");

        let call = tokio::time::timeout(
            Duration::from_secs(10),
            bridge.call("status.get", serde_json::json!({})),
        )
        .await
        .expect("refused at once, not left waiting");
        assert!(matches!(call, Err(BridgeError::Closed)), "{call:?}");
        drop(bridge);
        drain.await.unwrap();
    }
}
