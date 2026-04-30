use crate::mcp::notifications::spawn_notification_forwarder;
use crate::mcp::server::GelfMcpServer;
use crate::rpc::transport;
use crate::rpc::types::*;
use crate::shim::auto_start::DaemonConnection;
use rmcp::ServiceExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, oneshot};

pub struct DaemonBridge {
    writer: Mutex<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>,
    next_id: AtomicU64,
    notification_tx: broadcast::Sender<RpcNotification>,
}

impl DaemonBridge {
    pub fn new(writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send>) -> Self {
        let (notification_tx, _) = broadcast::channel(100);
        Self {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            notification_tx,
        }
    }

    /// Send an RPC request and wait for the response.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = RpcRequest::new(id, method, params);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        {
            let mut writer = self.writer.lock().await;
            transport::write_message(&mut *writer, &request).await?;
        }
        let response = rx.await.map_err(|_| anyhow::anyhow!("response channel closed"))?;
        match response.error {
            Some(err) => anyhow::bail!("{}", err.message),
            None => Ok(response.result.unwrap_or(serde_json::Value::Null)),
        }
    }

    /// Subscribe to notifications from the daemon.
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<RpcNotification> {
        self.notification_tx.subscribe()
    }
}

/// Spawns a reader loop that routes responses to pending callers and
/// broadcasts notifications to subscribers.
pub fn spawn_reader_loop(
    bridge: Arc<DaemonBridge>,
    reader: Box<dyn tokio::io::AsyncBufRead + Unpin + Send>,
) {
    tokio::spawn(async move {
        let mut reader = reader;
        loop {
            match transport::read_daemon_message(&mut reader).await {
                Ok(Some(DaemonMessage::Response(resp))) => {
                    if let Some(tx) = bridge.pending.lock().await.remove(&resp.id) {
                        let _ = tx.send(resp);
                    }
                }
                Ok(Some(DaemonMessage::Notification(notif))) => {
                    let _ = bridge.notification_tx.send(notif);
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("RPC read error: {e}");
                    break;
                }
            }
        }
    });
}

/// Connect to the daemon, perform session handshake, start the MCP server
/// on stdio, and forward daemon notifications.
pub async fn run_shim(
    conn: DaemonConnection,
    session_name: Option<String>,
) -> anyhow::Result<()> {
    let (reader, writer) = tokio::io::split(conn.stream);
    let bridge = Arc::new(DaemonBridge::new(Box::new(writer)));
    let buf_reader = tokio::io::BufReader::new(reader);
    spawn_reader_loop(bridge.clone(), Box::new(buf_reader));

    // Send session.start
    let start_result = bridge
        .call(
            "session.start",
            serde_json::to_value(&SessionStartParams {
                name: session_name,
                protocol_version: PROTOCOL_VERSION,
            })?,
        )
        .await?;
    let session_info: SessionStartResult = serde_json::from_value(start_result)?;
    eprintln!(
        "Connected to logmon daemon: session={}, new={}",
        session_info.session_id, session_info.is_new
    );

    // Create MCP server with bridge
    let server = GelfMcpServer::new_with_bridge(bridge.clone());
    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let running = server.serve(transport).await?;

    // Forward daemon notifications to MCP client
    let notification_rx = bridge.subscribe_notifications();
    spawn_notification_forwarder(notification_rx, running.peer().clone());

    running.waiting().await?;
    Ok(())
}
