use crate::transport;
use logmon_broker_protocol::*;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, oneshot, Mutex};

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
/// The reader half runs in a background task spawned by [`DaemonBridge::spawn`].
pub struct DaemonBridge {
    writer: Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>>,
    next_id: AtomicU64,
    notification_tx: broadcast::Sender<RpcNotification>,
}

impl DaemonBridge {
    /// Splits `stream` into reader/writer halves, spawns the reader loop, and
    /// returns a bridge that routes calls and forwards notifications onto
    /// `notification_tx`.
    pub async fn spawn<S>(
        stream: S,
        notification_tx: broadcast::Sender<RpcNotification>,
    ) -> Result<Self, BridgeError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let buf_reader = tokio::io::BufReader::new(reader);
        spawn_reader_loop(buf_reader, pending.clone(), notification_tx.clone());

        Ok(Self {
            writer: Mutex::new(Box::new(writer)),
            pending,
            next_id: AtomicU64::new(1),
            notification_tx,
        })
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

    /// Subscribe to daemon-originated notifications.
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<RpcNotification> {
        self.notification_tx.subscribe()
    }
}

/// Reader loop: routes responses to the pending-call map and broadcasts
/// notifications. Exits cleanly on EOF or transport error.
fn spawn_reader_loop<R>(
    mut reader: R,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>>,
    notification_tx: broadcast::Sender<RpcNotification>,
) where
    R: tokio::io::AsyncBufRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match transport::read_daemon_message(&mut reader).await {
                Ok(Some(DaemonMessage::Response(resp))) => {
                    if let Some(tx) = pending.lock().await.remove(&resp.id) {
                        let _ = tx.send(resp);
                    }
                }
                Ok(Some(DaemonMessage::Notification(notif))) => {
                    let _ = notification_tx.send(notif);
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("RPC read error: {e}");
                    break;
                }
            }
        }
    });
}
