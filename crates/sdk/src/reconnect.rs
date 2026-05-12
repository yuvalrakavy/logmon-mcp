//! Connection state machine for the broker SDK.
//!
//! Owns the live [`DaemonBridge`] (when one exists) plus the state-transition
//! logic that drives recovery from transient daemon disconnects.
//!
//! ## States
//!
//! - `Connected(bridge)` — the bridge is alive; calls flow through it.
//! - `Reconnecting` — the previous bridge died (EOF or transport error) and a
//!   background task is retrying the handshake with exponential backoff.
//!   In-flight calls block on `state_changed.notified()` until the deadline
//!   expires.
//! - `PermanentlyFailed(reason)` — terminal. Either the session can't be
//!   resumed (anonymous, or daemon resurrection produced `is_new: true`), or
//!   we exhausted [`BrokerBuilder::reconnect_max_attempts`].
//!
//! ## Anonymous sessions
//!
//! There's no way to resume an anonymous session across daemon restart, so any
//! disconnect on an anonymous broker transitions directly to
//! `PermanentlyFailed(SessionLost)` — never to `Reconnecting`.
//!
//! ## Notification ordering
//!
//! `Notification::Reconnected` is broadcast AFTER the new session.start
//! handshake succeeds but BEFORE the new bridge's reader task starts processing
//! daemon-drained queue notifications. This is achieved with
//! [`DaemonBridge::spawn_without_reader`] + [`DaemonBridge::start_reader`]:
//! the connect path runs the handshake on its own task, broadcasts
//! `Reconnected`, then starts the reader.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{broadcast, Mutex, Notify};

use crate::bridge::{BridgeError, DaemonBridge, PendingReader};
use crate::connect::{default_socket_path, BrokerBuilder};
use crate::{BrokerError, Notification};
use logmon_broker_protocol::{
    parse_daemon_message_from_str, DaemonMessage, RpcRequest, RpcResponse, SessionStartParams,
    SessionStartResult, PROTOCOL_VERSION,
};

/// Reason for entering [`ConnectionState::PermanentlyFailed`]. Stored as a
/// small `Copy` enum because `BrokerError` is not `Clone` (its `Transport`
/// variant wraps `io::Error`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum TerminalReason {
    /// Session can't be resumed: either anonymous (no name to reconnect with)
    /// or the daemon came back fresh (`is_new: true` on a named reconnect).
    SessionLost,
    /// All `reconnect_max_attempts` exhausted.
    Disconnected,
}

impl TerminalReason {
    pub(crate) fn into_error(self) -> BrokerError {
        match self {
            TerminalReason::SessionLost => BrokerError::SessionLost,
            TerminalReason::Disconnected => BrokerError::Disconnected,
        }
    }
}

pub(crate) enum ConnectionState {
    Connected(Arc<DaemonBridge>),
    Reconnecting,
    PermanentlyFailed(TerminalReason),
}

/// Owns the connection state machine for one broker session.
pub(crate) struct ConnectionManager {
    pub(crate) state: Mutex<ConnectionState>,
    pub(crate) state_changed: Notify,
    pub(crate) notification_tx: broadcast::Sender<Notification>,
    pub(crate) config: Arc<BrokerBuilder>,
    pub(crate) session_name: Option<String>,
    pub(crate) client_info: Option<Value>,
    pub(crate) session_id: String,
    pub(crate) is_new_session: bool,
    pub(crate) capabilities: Vec<String>,
    pub(crate) daemon_uptime: Duration,
}

impl ConnectionManager {
    /// Returns the live bridge, blocking until reconnect succeeds or the
    /// deadline expires. Returns `Err` if reconnect is exhausted, the session
    /// is unrecoverable, or the deadline passes while waiting.
    pub(crate) async fn current_bridge(
        self: &Arc<Self>,
        deadline: tokio::time::Instant,
    ) -> Result<Arc<DaemonBridge>, BrokerError> {
        loop {
            // Subscribe to state changes BEFORE checking the state — this
            // closes the race where the state flips from Reconnecting to
            // Connected between our check and our await on `notified()`.
            let notified = self.state_changed.notified();
            tokio::pin!(notified);

            {
                let state = self.state.lock().await;
                match &*state {
                    ConnectionState::Connected(bridge) => return Ok(bridge.clone()),
                    ConnectionState::PermanentlyFailed(reason) => {
                        return Err(reason.into_error());
                    }
                    ConnectionState::Reconnecting => {
                        // Fall through to wait below
                    }
                }
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(BrokerError::Disconnected);
            }
            tokio::select! {
                _ = &mut notified => continue,
                _ = tokio::time::sleep(remaining) => {
                    return Err(BrokerError::Disconnected);
                }
            }
        }
    }

    /// Called when the underlying bridge reports EOF/transport error. Drives
    /// the transition into either `Reconnecting` (named session) or
    /// `PermanentlyFailed(SessionLost)` (anonymous session). Idempotent:
    /// repeated calls during an in-flight reconnect are no-ops.
    pub(crate) async fn enter_disconnect(self: &Arc<Self>) {
        let mut state = self.state.lock().await;
        match &*state {
            // Already handling — no-op.
            ConnectionState::Reconnecting | ConnectionState::PermanentlyFailed(_) => return,
            ConnectionState::Connected(_) => {}
        }

        if self.session_name.is_none() {
            // Anonymous session can't be resumed.
            *state = ConnectionState::PermanentlyFailed(TerminalReason::SessionLost);
            drop(state);
            self.state_changed.notify_waiters();
            tracing::warn!("anonymous session disconnected; transitioning to SessionLost");
            return;
        }

        *state = ConnectionState::Reconnecting;
        drop(state);
        self.state_changed.notify_waiters();
        tracing::info!(
            session_name = ?self.session_name,
            "named session disconnected; entering Reconnecting"
        );

        let mgr = self.clone();
        tokio::spawn(async move {
            reconnect_loop(mgr).await;
        });
    }
}

async fn reconnect_loop(mgr: Arc<ConnectionManager>) {
    let mut backoff = mgr.config.resolved_initial_backoff();
    let max_backoff = mgr.config.resolved_max_backoff();
    let max_attempts = mgr.config.resolved_max_attempts();

    tracing::debug!(
        max_attempts,
        initial_backoff_ms = backoff.as_millis() as u64,
        "reconnect_loop starting"
    );

    for attempt in 0..max_attempts {
        match try_handshake(&mgr).await {
            Ok((bridge, pending_reader, is_new)) => {
                tracing::info!(attempt, is_new, "reconnect handshake succeeded");
                if is_new {
                    // Daemon resurrection — the named session we held is gone.
                    let mut state = mgr.state.lock().await;
                    *state = ConnectionState::PermanentlyFailed(TerminalReason::SessionLost);
                    drop(state);
                    mgr.state_changed.notify_waiters();
                    tracing::warn!(
                        "reconnect: daemon returned is_new=true, session resurrected; \
                         transitioning to SessionLost"
                    );
                    // Drop the bridge + pending_reader. The reader was never
                    // started, so the connection just closes cleanly.
                    drop(bridge);
                    drop(pending_reader);
                    return;
                }

                // Successful resume. Install the bridge in Connected state
                // FIRST (so any blocked callers can grab it), then broadcast
                // Reconnected, then start the reader. The reader-start ordering
                // ensures Reconnected reaches subscribers before the daemon's
                // drained-queue notifications get processed.
                let disconnect = bridge.disconnect_notify();
                let mut state = mgr.state.lock().await;
                *state = ConnectionState::Connected(bridge.clone());
                drop(state);
                mgr.state_changed.notify_waiters();
                let _ = mgr.notification_tx.send(Notification::Reconnected);
                bridge.start_reader(pending_reader);
                // Spawn a fresh disconnect watcher for THIS bridge so a second
                // disconnect drives another reconnect cycle (or terminal
                // SessionLost for anonymous, etc.).
                spawn_disconnect_watcher(mgr.clone(), disconnect);
                tracing::info!(attempt, "reconnect succeeded");
                return;
            }
            Err(e) => {
                tracing::warn!(attempt, error = %e, "reconnect attempt failed; backing off");
                let jitter = rand::random::<f32>() * 0.3 + 0.85; // 0.85..1.15
                let sleep_dur = Duration::from_secs_f32(backoff.as_secs_f32() * jitter);
                tokio::time::sleep(sleep_dur).await;
                backoff = std::cmp::min(backoff.saturating_mul(2), max_backoff);
            }
        }
    }

    // Exhausted attempts → terminal Disconnected.
    let mut state = mgr.state.lock().await;
    *state = ConnectionState::PermanentlyFailed(TerminalReason::Disconnected);
    drop(state);
    mgr.state_changed.notify_waiters();
    tracing::warn!(
        max_attempts = max_attempts,
        "reconnect exhausted attempts; transitioning to Disconnected"
    );
}

/// Connect, run a session.start handshake synchronously (without spawning a
/// reader task yet), and return the bridge + its [`PendingReader`] + the
/// `is_new` flag. The caller is responsible for starting the reader once
/// any ordering-sensitive work is done.
async fn try_handshake(
    mgr: &Arc<ConnectionManager>,
) -> Result<(Arc<DaemonBridge>, PendingReader, bool), BrokerError> {
    let socket_path = mgr
        .config
        .resolved_socket_path()
        .unwrap_or_else(default_socket_path);

    #[cfg(unix)]
    let stream = tokio::net::UnixStream::connect(&socket_path).await?;
    #[cfg(windows)]
    let stream = {
        let _ = &socket_path;
        tokio::net::TcpStream::connect("127.0.0.1:12200").await?
    };

    // Build the bridge but DON'T start its reader yet — we run the handshake
    // synchronously below, then ordering-sensitive work happens in the
    // reconnect_loop, then `start_reader` is invoked.
    let (bridge, mut pending_reader) =
        DaemonBridge::spawn_without_reader(stream, mgr.notification_tx.clone())
            .await
            .map_err(map_bridge_error)?;
    let bridge = Arc::new(bridge);

    // Synchronous session.start: write the request through the bridge's
    // writer, then read exactly one line off the pending reader. This
    // mirrors the pattern in `core::test_support::TestClient::try_connect`.
    let params = SessionStartParams {
        name: mgr.session_name.clone(),
        protocol_version: PROTOCOL_VERSION,
        client_info: mgr.client_info.clone(),
    };
    let req = RpcRequest::new(0, "session.start", serde_json::to_value(&params).unwrap());
    let req_json = serde_json::to_string(&req).map_err(|e| BrokerError::Protocol(e.to_string()))?;

    {
        use tokio::io::AsyncWriteExt;
        let mut writer = bridge.writer_lock().await;
        writer
            .write_all(req_json.as_bytes())
            .await
            .map_err(BrokerError::Transport)?;
        writer
            .write_all(b"\n")
            .await
            .map_err(BrokerError::Transport)?;
        writer.flush().await.map_err(BrokerError::Transport)?;
    }

    // Read one line off the pending reader. The daemon writes the
    // session.start response BEFORE any drained notifications, so the first
    // line we receive is the response. Drained notifications stay in the
    // pending_reader's buffered stream and will be processed after we call
    // `start_reader`.
    let response_line = read_one_line(&mut pending_reader).await?;
    let result = match parse_daemon_message_from_str(&response_line)
        .map_err(|e| BrokerError::Protocol(e.to_string()))?
    {
        DaemonMessage::Response(resp) => parse_session_start_response(resp)?,
        DaemonMessage::Notification(_) => {
            return Err(BrokerError::Protocol(
                "daemon sent a notification before session.start response".into(),
            ));
        }
    };

    Ok((bridge, pending_reader, result.is_new))
}

fn parse_session_start_response(resp: RpcResponse) -> Result<SessionStartResult, BrokerError> {
    if let Some(err) = resp.error {
        return Err(BrokerError::Method {
            code: err.code,
            message: err.message,
        });
    }
    let value = resp
        .result
        .ok_or_else(|| BrokerError::Protocol("session.start response had no result".into()))?;
    serde_json::from_value(value).map_err(|e| BrokerError::Protocol(e.to_string()))
}

async fn read_one_line(pending_reader: &mut PendingReader) -> Result<String, BrokerError> {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    let n = pending_reader
        .reader_mut()
        .read_line(&mut line)
        .await
        .map_err(BrokerError::Transport)?;
    if n == 0 {
        return Err(BrokerError::Protocol(
            "daemon closed connection before session.start response".into(),
        ));
    }
    Ok(line)
}

pub(crate) fn map_bridge_error(err: BridgeError) -> BrokerError {
    match err {
        BridgeError::Transport(e) => BrokerError::Transport(e),
        BridgeError::Rpc { code, message } => BrokerError::Method { code, message },
        BridgeError::Closed => BrokerError::Disconnected,
        BridgeError::Protocol(msg) => BrokerError::Protocol(msg),
    }
}

/// Discriminator: was a [`BridgeError`] a transport-class failure (which
/// means the bridge is dead and reconnect should fire), or a method-level
/// error (the bridge is fine, return as-is)?
pub(crate) fn is_transport_error(err: &BridgeError) -> bool {
    matches!(err, BridgeError::Transport(_) | BridgeError::Closed)
}

/// Build the initial connection: connect, handshake, install bridge.
pub(crate) async fn initial_connect(
    config: Arc<BrokerBuilder>,
) -> Result<Arc<ConnectionManager>, BrokerError> {
    let socket_path = config
        .resolved_socket_path()
        .unwrap_or_else(default_socket_path);
    let session_name = config.session_name_clone();
    let client_info = config.client_info_clone();
    let notification_buffer = config.notification_buffer();

    if let Some(ref ci) = client_info {
        let serialized =
            serde_json::to_string(ci).map_err(|e| BrokerError::Protocol(e.to_string()))?;
        if serialized.len() > 4096 {
            return Err(BrokerError::Method {
                code: -32602,
                message: "client_info exceeds 4 KB limit".into(),
            });
        }
    }

    #[cfg(unix)]
    let stream = tokio::net::UnixStream::connect(&socket_path).await?;
    #[cfg(windows)]
    let stream = {
        let _ = &socket_path;
        tokio::net::TcpStream::connect("127.0.0.1:12200").await?
    };

    let (notification_tx, _) = broadcast::channel(notification_buffer);
    let (bridge, mut pending_reader) =
        DaemonBridge::spawn_without_reader(stream, notification_tx.clone())
            .await
            .map_err(map_bridge_error)?;
    let bridge = Arc::new(bridge);

    let params = SessionStartParams {
        name: session_name.clone(),
        protocol_version: PROTOCOL_VERSION,
        client_info: client_info.clone(),
    };
    let req = RpcRequest::new(0, "session.start", serde_json::to_value(&params).unwrap());
    let req_json = serde_json::to_string(&req).map_err(|e| BrokerError::Protocol(e.to_string()))?;

    {
        use tokio::io::AsyncWriteExt;
        let mut writer = bridge.writer_lock().await;
        writer
            .write_all(req_json.as_bytes())
            .await
            .map_err(BrokerError::Transport)?;
        writer
            .write_all(b"\n")
            .await
            .map_err(BrokerError::Transport)?;
        writer.flush().await.map_err(BrokerError::Transport)?;
    }

    let response_line = read_one_line(&mut pending_reader).await?;
    let result = match parse_daemon_message_from_str(&response_line)
        .map_err(|e| BrokerError::Protocol(e.to_string()))?
    {
        DaemonMessage::Response(resp) => parse_session_start_response(resp)?,
        DaemonMessage::Notification(_) => {
            return Err(BrokerError::Protocol(
                "daemon sent a notification before session.start response".into(),
            ));
        }
    };

    let mgr = Arc::new(ConnectionManager {
        state: Mutex::new(ConnectionState::Connected(bridge.clone())),
        state_changed: Notify::new(),
        notification_tx,
        config,
        session_name,
        client_info,
        session_id: result.session_id,
        is_new_session: result.is_new,
        capabilities: result.capabilities.clone(),
        daemon_uptime: Duration::from_secs(result.daemon_uptime_secs),
    });

    // Start reader AFTER ConnectionManager is constructed so that the reader's
    // disconnect-watcher task can hold an Arc<ConnectionManager>.
    bridge.start_reader(pending_reader);

    // Spawn the disconnect watcher: wait for the bridge's disconnect signal,
    // then call enter_disconnect. The watcher task lives only until disconnect
    // fires; on next reconnect a new watcher is spawned for the new bridge.
    spawn_disconnect_watcher(mgr.clone(), bridge.disconnect_notify());

    Ok(mgr)
}

/// Watcher: when the bridge fires its disconnect notify, transition the
/// manager to Reconnecting (or PermanentlyFailed for anonymous). Spawned once
/// per live bridge.
pub(crate) fn spawn_disconnect_watcher(mgr: Arc<ConnectionManager>, disconnect: Arc<Notify>) {
    tokio::spawn(async move {
        disconnect.notified().await;
        mgr.enter_disconnect().await;
    });
}
