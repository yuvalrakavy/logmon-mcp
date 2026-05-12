//! In-process daemon harness for integration tests.
//!
//! Gated behind the `test-support` feature. The harness lets a test:
//!
//! - spawn a daemon on a tempdir socket without binding real GELF/OTLP
//!   listeners (`spawn_test_daemon`),
//! - inject synthetic [`LogEntry`] values directly into the pipeline
//!   ([`TestDaemonHandle::inject_log`]),
//! - pause/resume the accept loop to exercise reconnect mid-flight
//!   ([`TestDaemonHandle::pause_accept`] / [`TestDaemonHandle::resume_accept`]),
//! - kill+restart the daemon while preserving `state.json`
//!   ([`TestDaemonHandle::restart`]) or after wiping it
//!   ([`TestDaemonHandle::wipe_state_and_restart`]),
//! - connect a low-level RPC [`TestClient`] (anonymous or named, with optional
//!   `client_info`) and call methods / await notifications.
//!
//! The harness owns its own `TempDir` so each test is isolated; nothing is
//! written under `~/.config/logmon/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};

use logmon_broker_protocol::{
    parse_daemon_message_from_str, DaemonMessage, RpcNotification, RpcRequest, RpcResponse,
    SessionStartParams, SessionStartResult, PROTOCOL_VERSION,
};

use crate::daemon::persistence::DaemonConfig;
use crate::daemon::server::{run_with_overrides, DaemonOverrides};
use crate::gelf::message::{Level, LogEntry};

const SOCKET_WAIT_TICKS: usize = 100;
const SOCKET_WAIT_INTERVAL: Duration = Duration::from_millis(20);

/// Default config used by the test harness: every receiver port set to 0 so
/// nothing real is bound, and a small-but-realistic buffer.
fn default_test_config() -> DaemonConfig {
    DaemonConfig {
        gelf_port: 0,
        gelf_udp_port: None,
        gelf_tcp_port: None,
        buffer_size: 10_000,
        persist_buffer_on_exit: false,
        idle_timeout_secs: 1800,
        otlp_grpc_port: 0,
        otlp_http_port: 0,
        span_buffer_size: 1_000,
    }
}

/// Handle to an in-process daemon spawned by [`spawn_test_daemon`].
///
/// Dropping the handle cancels the spawned task; tests that want a clean
/// shutdown should call [`Self::shutdown`] (or simply let the test scope end).
pub struct TestDaemonHandle {
    /// Absolute path to the daemon's Unix socket. Connect to this from
    /// [`TestClient`] or any other client.
    pub socket_path: PathBuf,
    /// Tempdir backing the daemon's `state.json` / `daemon.pid` /
    /// `logmon.sock`. Held in an `Arc` so [`Self::restart`] can reuse it.
    pub tempdir: Arc<TempDir>,
    /// Resolved daemon config used at spawn time.
    pub config: DaemonConfig,

    log_tx: mpsc::Sender<LogEntry>,
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    accept_paused: Arc<AtomicBool>,
    join_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl TestDaemonHandle {
    /// Spawn an in-process daemon on a fresh tempdir socket. One per test.
    pub async fn spawn() -> Self {
        let tempdir = Arc::new(TempDir::new().expect("create tempdir for test daemon"));
        Self::spawn_in_dir(tempdir, default_test_config()).await
    }

    /// Spawn an in-process daemon reusing a caller-provided tempdir. Lets a
    /// test pre-populate `state.json` (or other config files) inside the
    /// tempdir before the daemon boots — used to verify load-from-disk paths
    /// like persisted-trigger restoration.
    pub async fn spawn_in_tempdir(tempdir: Arc<TempDir>) -> Self {
        Self::spawn_in_dir(tempdir, default_test_config()).await
    }

    /// Spawn an in-process daemon reusing an existing tempdir. Used by
    /// [`Self::restart`] and [`Self::wipe_state_and_restart`] to preserve (or
    /// drop and rebuild) `state.json`.
    async fn spawn_in_dir(tempdir: Arc<TempDir>, config: DaemonConfig) -> Self {
        let socket_path = tempdir.path().join("logmon.sock");
        let dir_for_daemon = tempdir.path().to_path_buf();
        let socket_path_for_daemon = socket_path.clone();

        let (log_tx, log_rx) = mpsc::channel::<LogEntry>(1024);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let accept_paused = Arc::new(AtomicBool::new(false));
        let accept_paused_for_daemon = accept_paused.clone();
        let config_for_daemon = config.clone();

        let join_handle = tokio::spawn(async move {
            let overrides = DaemonOverrides {
                config_dir: Some(dir_for_daemon),
                socket_path: Some(socket_path_for_daemon),
                injected_log_rx: Some(log_rx),
                shutdown_rx: Some(shutdown_rx),
                accept_paused: Some(accept_paused_for_daemon),
                skip_tracing_init: true,
            };
            if let Err(e) = run_with_overrides(config_for_daemon, overrides).await {
                eprintln!("test daemon exited with error: {e}");
            }
        });

        // Wait for the socket to appear so callers can connect immediately.
        let mut appeared = false;
        for _ in 0..SOCKET_WAIT_TICKS {
            if socket_path.exists() {
                appeared = true;
                break;
            }
            tokio::time::sleep(SOCKET_WAIT_INTERVAL).await;
        }
        assert!(
            appeared,
            "test daemon socket {} never appeared",
            socket_path.display()
        );

        Self {
            socket_path,
            tempdir,
            config,
            log_tx,
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            accept_paused,
            join_handle: Mutex::new(Some(join_handle)),
        }
    }

    /// Inject a synthetic log entry directly into the pipeline, bypassing
    /// GELF/OTLP. The entry's `seq` is reassigned by the pipeline.
    pub async fn inject_log(&self, level: Level, message: &str) {
        let entry = LogEntry::synthetic(level, message);
        let _ = self.log_tx.send(entry).await;
    }

    /// Pause the accept loop. While paused, the daemon skips the `accept()`
    /// call and yields for 50 ms per tick; the listener stays bound, so new
    /// `connect_*` calls queue in the kernel backlog and complete once
    /// [`Self::resume_accept`] is called.
    pub async fn pause_accept(&self) {
        self.accept_paused.store(true, Ordering::SeqCst);
    }

    /// Resume the accept loop after [`Self::pause_accept`].
    pub async fn resume_accept(&self) {
        self.accept_paused.store(false, Ordering::SeqCst);
    }

    /// Shut down the daemon and wait for its task to finish. Subsequent
    /// methods that rely on a running daemon will fail.
    ///
    /// The await on the daemon's `JoinHandle` is bounded by a 5 s timeout. If
    /// the task hasn't resolved by then we abort it and proceed — a hung
    /// shutdown must not deadlock the test runner. A `tracing::warn!` fires on
    /// abort so flaky-test triage has a signal.
    pub async fn shutdown(&self) {
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.join_handle.lock().await.take() {
            let abort_handle = handle.abort_handle();
            if tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .is_err()
            {
                tracing::warn!("test daemon did not shut down within 5s; aborting task");
                abort_handle.abort();
            }
        }
        // Wait for the socket to disappear (best-effort).
        for _ in 0..SOCKET_WAIT_TICKS {
            if !self.socket_path.exists() {
                break;
            }
            tokio::time::sleep(SOCKET_WAIT_INTERVAL).await;
        }
    }

    /// Kill + restart the daemon, preserving `state.json` in the tempdir.
    pub async fn restart(&mut self) {
        self.shutdown().await;
        let new = Self::spawn_in_dir(self.tempdir.clone(), self.config.clone()).await;
        self.absorb(new).await;
    }

    /// Kill the daemon, wipe `state.json`, then restart it. Used to test
    /// fresh-start behavior.
    pub async fn wipe_state_and_restart(&mut self) {
        self.shutdown().await;
        let state_path = self.tempdir.path().join("state.json");
        let _ = std::fs::remove_file(&state_path);
        let new = Self::spawn_in_dir(self.tempdir.clone(), self.config.clone()).await;
        self.absorb(new).await;
    }

    /// Move runtime state from a freshly-spawned handle into `self`. The
    /// freshly-spawned handle is consumed; it shared the same `Arc<TempDir>`
    /// as `self`, so the tempdir survives the swap. We work through `&mut`
    /// borrows because [`TestDaemonHandle`] implements [`Drop`], which
    /// disallows moving fields out of a value.
    async fn absorb(&mut self, new: Self) {
        // Tear down the new handle's Drop side-effect before consuming its
        // fields: pull its shutdown sender out so `Drop::drop` can't fire it
        // when `new` goes out of scope.
        let new_shutdown = new.shutdown_tx.lock().await.take();
        let new_join = new.join_handle.lock().await.take();
        let new_socket = new.socket_path.clone();
        let new_config = new.config.clone();
        let new_log_tx = new.log_tx.clone();
        let new_paused = new.accept_paused.clone();
        // Drop `new` explicitly — its Drop is a no-op now that shutdown_tx is
        // empty.
        drop(new);

        self.socket_path = new_socket;
        self.config = new_config;
        self.log_tx = new_log_tx;
        *self.shutdown_tx.lock().await = new_shutdown;
        *self.join_handle.lock().await = new_join;
        self.accept_paused = new_paused;
    }

    /// Connect an anonymous client (no session name).
    pub async fn connect_anon(&self) -> TestClient {
        TestClient::connect(&self.socket_path, None, None).await
    }

    /// Connect a named client. `client_info` is forwarded into
    /// [`SessionStartParams::client_info`].
    pub async fn connect_named(&self, name: &str, client_info: Option<Value>) -> TestClient {
        TestClient::connect(&self.socket_path, Some(name.to_string()), client_info).await
    }

    /// Like [`Self::connect_named`] but propagates errors instead of
    /// panicking. Used for negative tests (bad version, name collision, ...).
    pub async fn try_connect_named(
        &self,
        name: &str,
        client_info: Option<Value>,
    ) -> anyhow::Result<TestClient> {
        TestClient::try_connect(&self.socket_path, Some(name.to_string()), client_info).await
    }

    /// Spawn the daemon with the REAL GELF receiver (UDP+TCP bound to
    /// kernel-assigned ports). Required for tests that exercise the actual
    /// receiver paths. `spawn()` uses the injected-channel harness which
    /// disables real receivers and is unsuitable for backpressure tests.
    pub async fn spawn_with_real_receivers() -> Self {
        let tempdir = Arc::new(TempDir::new().expect("create tempdir for test daemon"));
        Self::spawn_in_dir_no_inject(tempdir, default_test_config()).await
    }

    async fn spawn_in_dir_no_inject(tempdir: Arc<TempDir>, config: DaemonConfig) -> Self {
        let socket_path = tempdir.path().join("logmon.sock");
        let dir_for_daemon = tempdir.path().to_path_buf();
        let socket_path_for_daemon = socket_path.clone();

        // log_tx is unused for real-receiver tests, but the struct field
        // must be initialised; keep a dropped channel so inject_log() (if
        // ever called) becomes a silent no-op.
        let (log_tx, _drop_log_rx) = mpsc::channel::<LogEntry>(1);
        drop(_drop_log_rx);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let accept_paused = Arc::new(AtomicBool::new(false));
        let accept_paused_for_daemon = accept_paused.clone();
        let config_for_daemon = config.clone();

        let join_handle = tokio::spawn(async move {
            let overrides = DaemonOverrides {
                config_dir: Some(dir_for_daemon),
                socket_path: Some(socket_path_for_daemon),
                injected_log_rx: None, // <-- key difference from spawn_in_dir
                shutdown_rx: Some(shutdown_rx),
                accept_paused: Some(accept_paused_for_daemon),
                skip_tracing_init: true,
            };
            if let Err(e) = run_with_overrides(config_for_daemon, overrides).await {
                eprintln!("test daemon (real receivers) exited with error: {e}");
            }
        });

        let mut appeared = false;
        for _ in 0..SOCKET_WAIT_TICKS {
            if socket_path.exists() {
                appeared = true;
                break;
            }
            tokio::time::sleep(SOCKET_WAIT_INTERVAL).await;
        }
        assert!(
            appeared,
            "real-receiver daemon socket {} never appeared",
            socket_path.display()
        );

        Self {
            socket_path,
            tempdir,
            config,
            log_tx,
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            accept_paused,
            join_handle: Mutex::new(Some(join_handle)),
        }
    }

    /// Discover the bound UDP port of the GELF receiver via a status.get
    /// RPC call. Only meaningful for daemons spawned with
    /// `spawn_with_real_receivers`.
    pub async fn gelf_udp_port(&self) -> u16 {
        use logmon_broker_protocol::StatusGetResult;
        use serde_json::json;

        let mut client = self.connect_anon().await;
        let result: StatusGetResult = client
            .call("status.get", json!({}))
            .await
            .expect("status.get must succeed on freshly-spawned daemon");
        for entry in &result.receivers {
            if let Some(rest) = entry.strip_prefix("UDP:") {
                if let Ok(port) = rest.parse::<u16>() {
                    return port;
                }
            }
        }
        panic!("no UDP receiver in receivers list: {:?}", result.receivers)
    }

    /// Run a session.start handshake and immediately drop the connection,
    /// returning just the handshake result.
    pub async fn session_start(
        &self,
        name: Option<&str>,
        client_info: Option<Value>,
    ) -> anyhow::Result<SessionStartResult> {
        TestClient::session_start_only(&self.socket_path, name.map(String::from), client_info).await
    }
}

impl Drop for TestDaemonHandle {
    fn drop(&mut self) {
        // Best-effort shutdown so the daemon doesn't outlive the test.
        if let Some(tx) = self.shutdown_tx.get_mut().take() {
            let _ = tx.send(());
        }
    }
}

/// Low-level RPC client used by harness tests. Owns one Unix-socket connection
/// to the daemon, dispatches responses by id, and surfaces notifications
/// through an unbounded mpsc.
pub struct TestClient {
    writer: tokio::io::WriteHalf<UnixStream>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>>,
    notification_rx: mpsc::UnboundedReceiver<RpcNotification>,
    next_id: AtomicU64,
    /// The handshake response. Populated once during connect and never
    /// mutated thereafter.
    pub session_start_result: SessionStartResult,
}

impl TestClient {
    /// Connect + handshake. Panics on failure — use [`Self::try_connect`] for
    /// negative tests.
    pub async fn connect(
        socket_path: &Path,
        name: Option<String>,
        client_info: Option<Value>,
    ) -> Self {
        Self::try_connect(socket_path, name, client_info)
            .await
            .expect("test client failed to connect")
    }

    /// Connect + handshake, propagating errors.
    ///
    /// The handshake is run BEFORE spawning the reader task: we register a
    /// pending slot for id=0, write `session.start`, read exactly one line,
    /// parse it as the handshake response, then spawn the reader task on the
    /// remaining stream. This avoids races between the reader and the
    /// handshake-await branch.
    pub async fn try_connect(
        socket_path: &Path,
        name: Option<String>,
        client_info: Option<Value>,
    ) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(socket_path).await?;
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);

        // Send session.start with id=0.
        let params = SessionStartParams {
            name,
            protocol_version: PROTOCOL_VERSION,
            client_info,
        };
        let req = RpcRequest::new(0, "session.start", serde_json::to_value(&params)?);
        let json = serde_json::to_string(&req)?;
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        // Read the handshake response synchronously. The daemon may also queue
        // notifications after the response (drained from a previous session);
        // surface them through the notification channel once we hand reading
        // off to the reader task.
        let mut early_notifications: Vec<RpcNotification> = Vec::new();
        let session_start_result: SessionStartResult = loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                anyhow::bail!("daemon closed connection before session.start response");
            }
            match parse_daemon_message_from_str(&line)? {
                DaemonMessage::Response(resp) if resp.id == 0 => {
                    if let Some(err) = resp.error {
                        anyhow::bail!("session.start failed: {} ({})", err.message, err.code);
                    }
                    let result = resp
                        .result
                        .ok_or_else(|| anyhow::anyhow!("session.start response had no result"))?;
                    break serde_json::from_value(result)?;
                }
                DaemonMessage::Response(resp) => {
                    anyhow::bail!(
                        "unexpected response id {} before handshake completed",
                        resp.id
                    );
                }
                DaemonMessage::Notification(notif) => {
                    early_notifications.push(notif);
                }
            }
        };

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, notif_rx) = mpsc::unbounded_channel();

        // Forward any notifications that arrived before/with the handshake.
        for notif in early_notifications {
            let _ = notif_tx.send(notif);
        }

        // Spawn the reader task to dispatch responses + forward notifications.
        let pending_for_reader = pending.clone();
        tokio::spawn(async move {
            loop {
                let mut line = String::new();
                let n = match reader.read_line(&mut line).await {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if n == 0 {
                    break;
                }
                match parse_daemon_message_from_str(&line) {
                    Ok(DaemonMessage::Response(resp)) => {
                        if let Some(tx) = pending_for_reader.lock().await.remove(&resp.id) {
                            let _ = tx.send(resp);
                        }
                    }
                    Ok(DaemonMessage::Notification(notif)) => {
                        if notif_tx.send(notif).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            writer,
            pending,
            notification_rx: notif_rx,
            next_id: AtomicU64::new(1),
            session_start_result,
        })
    }

    /// Run a session.start handshake, then drop the connection. Returns just
    /// the handshake response — useful for capability/probe tests.
    pub async fn session_start_only(
        socket_path: &Path,
        name: Option<String>,
        client_info: Option<Value>,
    ) -> anyhow::Result<SessionStartResult> {
        let client = Self::try_connect(socket_path, name, client_info).await?;
        Ok(client.session_start_result.clone())
    }

    /// Explicitly close the connection to the daemon. This will cause the server
    /// to detect EOF and clean up the session. Useful for testing session cleanup behavior.
    pub async fn close(mut self) -> anyhow::Result<()> {
        self.writer.shutdown().await?;
        Ok(())
    }

    /// Call a method, await the response, and deserialize the result into
    /// `R`. Returns `Err` if the daemon returned a JSON-RPC error.
    ///
    /// On any error between inserting the pending entry and receiving the
    /// response, the entry is removed from the `pending` map so it doesn't
    /// leak across the connection's lifetime.
    pub async fn call<R: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> anyhow::Result<R> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = RpcRequest::new(id, method, params);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let result: anyhow::Result<RpcResponse> = async {
            let json = serde_json::to_string(&req)?;
            self.writer.write_all(json.as_bytes()).await?;
            self.writer.write_all(b"\n").await?;
            self.writer.flush().await?;
            rx.await
                .map_err(|e| anyhow::anyhow!("response channel closed: {e}"))
        }
        .await;

        match result {
            Ok(response) => match response.error {
                Some(err) => {
                    anyhow::bail!("rpc error {}: {}", err.code, err.message)
                }
                None => Ok(serde_json::from_value(
                    response.result.unwrap_or(Value::Null),
                )?),
            },
            Err(e) => {
                self.pending.lock().await.remove(&id);
                Err(e)
            }
        }
    }

    /// Wait up to ~2 s for a `trigger.fired` notification matching
    /// `trigger_id`. Other notifications are skipped silently. Panics on
    /// timeout.
    pub async fn expect_trigger_fired(&mut self, trigger_id: u32) -> RpcNotification {
        let timeout = Duration::from_secs(2);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, self.notification_rx.recv()).await {
                Ok(Some(notif))
                    if notif.method == "trigger.fired" || notif.method.contains("trigger_fired") =>
                {
                    let id_matches = notif
                        .params
                        .get("trigger_id")
                        .and_then(|v| v.as_u64())
                        == Some(trigger_id as u64);
                    if id_matches {
                        return notif;
                    }
                }
                Ok(Some(_)) => continue,
                Ok(None) => panic!(
                    "notification channel closed before trigger.fired for id {trigger_id} (daemon disconnected?)"
                ),
                Err(_) => panic!(
                    "timeout waiting for trigger.fired for id {trigger_id} (waited {timeout:?})"
                ),
            }
        }
    }

    /// Try to receive a notification within `timeout`. Returns `None` on
    /// timeout or when the channel is closed.
    pub async fn try_recv_notification(&mut self, timeout: Duration) -> Option<RpcNotification> {
        tokio::time::timeout(timeout, self.notification_rx.recv())
            .await
            .ok()
            .flatten()
    }
}

/// Convenience wrapper for the most common case: spawn a fresh in-process
/// daemon on a tempdir socket.
pub async fn spawn_test_daemon() -> TestDaemonHandle {
    TestDaemonHandle::spawn().await
}

/// Alias used by the SDK's integration tests. Identical to
/// [`spawn_test_daemon`]; the rename keeps the SDK-facing call site
/// self-documenting ("for sdk") without forking the harness implementation.
pub async fn spawn_test_daemon_for_sdk() -> TestDaemonHandle {
    spawn_test_daemon().await
}
