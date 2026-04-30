use crate::daemon::log_processor::{spawn_log_processor, sync_pre_buffer_size};
use crate::daemon::persistence::{
    config_dir, load_state, save_state, DaemonConfig, DaemonState, SEQ_BLOCK_SIZE,
};
use crate::daemon::rpc_handler::RpcHandler;
use crate::daemon::session::{SessionId, SessionRegistry};
use crate::daemon::span_processor::spawn_span_processor;
use crate::daemon::transport::{read_request, write_message};
use crate::engine::pipeline::{LogPipeline, PipelineEvent};
use crate::engine::seq_counter::SeqCounter;
use crate::gelf::message::LogEntry;
use crate::receiver::gelf::{GelfReceiver, GelfReceiverConfig};
use crate::receiver::otlp::{OtlpReceiver, OtlpReceiverConfig};
use crate::receiver::Receiver;
use crate::span::store::SpanStore;
use logmon_broker_protocol::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

/// Notification method name for fired triggers. Underscore form matches
/// `notifications/<name>` rationalization (vs the legacy dot form).
const TRIGGER_FIRED_METHOD: &str = "trigger_fired";

/// Convert an internal [`PipelineEvent`] (engine-side observability struct)
/// into a wire-shape JSON value matching
/// [`logmon_broker_protocol::TriggerFiredPayload`].
///
/// Engine-only fields (`pre_trigger_flushed`, `trace_id`, `trace_summary`)
/// are intentionally dropped — the v1 protocol payload does not include them.
/// The JSON shape of the embedded `LogEntry` matches the protocol's
/// `LogEntry` (gelf hex-encodes `trace_id`/`span_id` to strings via custom
/// serde, so the wire bytes are identical).
fn pipeline_event_to_trigger_fired(
    ev: &PipelineEvent,
) -> Result<serde_json::Value, serde_json::Error> {
    Ok(serde_json::json!({
        "trigger_id": ev.trigger_id,
        "description": ev.trigger_description,
        "filter_string": ev.filter_string,
        "pre_window": ev.pre_window,
        "post_window": ev.post_window_size,
        "notify_context": ev.notify_context,
        "oneshot": ev.oneshot,
        "matched_entry": serde_json::to_value(&ev.matched_entry)?,
        "context_before": serde_json::to_value(&ev.context_before)?,
    }))
}

/// Send a UDP multicast beacon to notify tracing-init circuit breakers
/// about OTel collector availability. Best-effort — failures are silently ignored.
fn send_otel_beacon(message: &str) {
    use std::net::UdpSocket;
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        let _ = socket.send_to(message.as_bytes(), "239.255.77.1:4399");
    }
}

/// Wait for either SIGTERM or SIGINT (Unix), or `ctrl_c` (Windows).
/// Production path uses this; the test harness injects a oneshot channel
/// via [`DaemonOverrides::shutdown_rx`] instead.
///
/// SIGTERM is what `systemctl stop` and `launchctl bootout` send by default.
/// SIGINT is what an interactive `Ctrl-C` sends. Both should drain cleanly.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "failed to install SIGTERM handler; falling back to ctrl_c only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        let mut intr = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "failed to install SIGINT handler; awaiting SIGTERM only");
                term.recv().await;
                info!("received SIGTERM");
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => info!("received SIGTERM"),
            _ = intr.recv() => info!("received SIGINT"),
        }
    }
    #[cfg(windows)]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!("received ctrl_c");
    }
}

/// Optional overrides applied by `run_with_overrides`. Used by the in-process
/// test harness (and only the test harness) to drive the daemon without a real
/// filesystem layout, real GELF/OTLP receivers, or `ctrl_c` shutdown.
#[derive(Default)]
pub struct DaemonOverrides {
    /// If `Some`, use this directory for `state.json` / `daemon.pid` /
    /// `logmon.sock` / `daemon.log` instead of `config_dir()`.
    pub config_dir: Option<PathBuf>,
    /// If `Some`, bind the Unix socket here instead of `<dir>/logmon.sock`.
    pub socket_path: Option<PathBuf>,
    /// If `Some`, take logs from this channel instead of starting GELF/OTLP
    /// receivers. When set, both the GELF receiver and the OTLP receiver are
    /// skipped entirely — no UDP/TCP/gRPC/HTTP listeners are bound.
    pub injected_log_rx: Option<mpsc::Receiver<LogEntry>>,
    /// If `Some`, await this for shutdown instead of `tokio::signal::ctrl_c()`.
    pub shutdown_rx: Option<oneshot::Receiver<()>>,
    /// If `Some`, the accept loop pauses (sleeps 50 ms and continues) while
    /// this atomic is `true`. Used to test reconnect mid-flight.
    pub accept_paused: Option<Arc<AtomicBool>>,
    /// If `true`, skip installing the daily-rotation tracing subscriber.
    /// Tests own their own subscriber (or none).
    pub skip_tracing_init: bool,
}

/// Run the logmon daemon. This function blocks until the daemon is shut down.
pub async fn run_daemon(config: DaemonConfig) -> anyhow::Result<()> {
    run_with_overrides(config, DaemonOverrides::default()).await
}

/// Run the logmon daemon with optional overrides. Production code calls
/// [`run_daemon`]; integration tests call this directly with a populated
/// [`DaemonOverrides`].
pub async fn run_with_overrides(
    config: DaemonConfig,
    overrides: DaemonOverrides,
) -> anyhow::Result<()> {
    let DaemonOverrides {
        config_dir: dir_override,
        socket_path: socket_override,
        injected_log_rx,
        shutdown_rx,
        accept_paused,
        skip_tracing_init,
    } = overrides;

    // 1. Resolve config dir
    let dir = dir_override.unwrap_or_else(config_dir);
    std::fs::create_dir_all(&dir)?;

    // 2. Set up file-based tracing (daily rotation), unless the harness owns
    //    its own subscriber. Done before the stale-pid sweep so its log line
    //    actually lands in `daemon.log`.
    let _tracing_guard = if skip_tracing_init {
        None
    } else {
        let file_appender = tracing_appender::rolling::daily(&dir, "daemon.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_target(true)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        Some(guard)
    };

    info!("logmon daemon starting");

    // 2b. Stale-pid sweep. If a `daemon.pid` exists from a previous run:
    //     - and that pid is alive, refuse to start (someone else owns the
    //       config dir; running two brokers concurrently would corrupt the
    //       socket and state file);
    //     - otherwise, remove the stale pid file AND the (now-unowned)
    //       socket file so we can start cleanly. This is what makes
    //       `kill -9 logmon-broker; logmon-broker` work without manual
    //       cleanup.
    //
    //     Tests use an injected, ephemeral config_dir per run, so any
    //     `daemon.pid` they encounter is a leftover from a crashed test —
    //     the same recovery semantics we want in production.
    let pid_path = dir.join("daemon.pid");
    let socket_path_for_sweep = socket_override
        .clone()
        .unwrap_or_else(|| dir.join("logmon.sock"));
    if pid_path.exists() {
        let pid_str = std::fs::read_to_string(&pid_path).unwrap_or_default();
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            if crate::daemon::process::is_process_alive(pid) {
                anyhow::bail!("another broker is already running (pid {pid}); abort");
            }
        }
        info!(?pid_path, "removing stale pid file from previous run");
        let _ = std::fs::remove_file(&pid_path);
        let _ = std::fs::remove_file(&socket_path_for_sweep);
    }
    drop(socket_path_for_sweep);

    // 3. Load state, get initial_seq from seq_block
    let state_path = dir.join("state.json");
    let state = load_state(&state_path)?;
    let initial_seq = state.seq_block;

    // 4. Create shared seq counter and pipeline
    let seq_counter = Arc::new(SeqCounter::new_with_initial(initial_seq));
    let pipeline = Arc::new(LogPipeline::new_with_seq_counter(
        config.buffer_size,
        seq_counter.clone(),
    ));

    // 5. Create SpanStore with shared seq counter
    let span_store = Arc::new(SpanStore::new(config.span_buffer_size, seq_counter.clone()));

    // 6. Reserve next seq block and save
    let reserved_seq_block = initial_seq + SEQ_BLOCK_SIZE;
    let new_state = DaemonState {
        seq_block: reserved_seq_block,
        named_sessions: state.named_sessions.clone(),
    };
    save_state(&state_path, &new_state)?;

    // 7. Create SessionRegistry, restore named sessions from state
    let sessions = Arc::new(SessionRegistry::new());
    for (name, persisted) in &state.named_sessions {
        sessions.restore_named(name, persisted);
    }

    // 8/9. Receivers — either inject a pre-built channel (test harness) or
    //      start the real GELF + OTLP receivers. Both paths spawn a span
    //      processor so the wiring is uniform; in the harness path nothing
    //      pushes into the span channel so the processor sits idle.
    let (log_rx, _otlp_receiver, all_receivers_info) = match injected_log_rx {
        Some(rx) => {
            info!("daemon running with injected log channel; GELF/OTLP receivers disabled");
            // Idle span channel — sender dropped immediately, processor will
            // exit cleanly when its recv() returns None.
            let (_span_tx, span_rx_idle) = mpsc::channel::<crate::span::types::SpanEntry>(1);
            drop(_span_tx);
            let _idle_span_processor = spawn_span_processor(
                span_rx_idle,
                span_store.clone(),
                sessions.clone(),
                pipeline.clone(),
            );
            (rx, None, Vec::<String>::new())
        }
        None => {
            // Real GELF receiver
            let (log_tx, log_rx) = mpsc::channel(1024);
            let (span_tx, span_rx_real) = mpsc::channel(1024);
            let udp_port = config.gelf_udp_port.unwrap_or(config.gelf_port);
            let tcp_port = config.gelf_tcp_port.unwrap_or(config.gelf_port);
            let gelf_config = GelfReceiverConfig {
                udp_addr: format!("0.0.0.0:{udp_port}"),
                tcp_addr: format!("0.0.0.0:{tcp_port}"),
            };
            let gelf_receiver = GelfReceiver::start(gelf_config, log_tx.clone()).await?;
            let mut all_receivers_info = gelf_receiver.listening_on();
            info!(?all_receivers_info, "GELF receiver started");

            // Optional OTLP receiver. Held alive for daemon lifetime — dropping
            // it closes the shutdown channel which signals the gRPC/HTTP
            // servers to stop.
            let otlp_receiver = if config.otlp_grpc_port > 0 || config.otlp_http_port > 0 {
                let otlp_config = OtlpReceiverConfig {
                    grpc_addr: format!("0.0.0.0:{}", config.otlp_grpc_port),
                    http_addr: format!("0.0.0.0:{}", config.otlp_http_port),
                };
                let otlp_receiver =
                    OtlpReceiver::start(otlp_config, log_tx.clone(), span_tx).await?;
                let otlp_info = otlp_receiver.listening_on();
                info!(?otlp_info, "OTLP receiver started");
                all_receivers_info.extend(otlp_info);
                send_otel_beacon("OTEL:ONLINE\n");
                Some(otlp_receiver)
            } else {
                // Drop span_tx so the span processor's recv() eventually
                // returns None when the daemon shuts down.
                drop(span_tx);
                None
            };
            // Spawn span processor with the real span channel.
            let _span_processor = spawn_span_processor(
                span_rx_real,
                span_store.clone(),
                sessions.clone(),
                pipeline.clone(),
            );
            (log_rx, otlp_receiver, all_receivers_info)
        }
    };

    // 10. Write PID file (path was resolved earlier in step 1b for the
    //     stale-pid sweep)
    std::fs::write(&pid_path, std::process::id().to_string())?;

    // 11. Start log processor
    let _processor_handle = spawn_log_processor(log_rx, pipeline.clone(), sessions.clone());

    // 12. Sync pre-buffer size after restoring sessions
    sync_pre_buffer_size(&pipeline, &sessions);

    // 13a. Create BookmarkStore (in-memory only, not persisted)
    let bookmark_store = Arc::new(crate::store::bookmarks::BookmarkStore::new());

    // 13. Create RpcHandler
    let handler = Arc::new(RpcHandler::new(
        pipeline.clone(),
        span_store.clone(),
        sessions.clone(),
        bookmark_store.clone(),
        all_receivers_info,
    ));

    // 14. Listen on Unix socket (unix) or TCP (windows)
    info!("daemon ready, listening for connections");

    // Notify systemd we are ready (Type=notify support). On non-Linux this
    // is a no-op at compile time. On Linux without `NOTIFY_SOCKET` set (i.e.
    // not running under systemd), `sd_notify::notify` returns an Err which
    // we log at debug — it is not a real failure.
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
            tracing::debug!(error = %e, "sd_notify ready failed (likely not running under systemd)");
        }
    }

    // Build a single shutdown future from either the override (test harness)
    // or `wait_for_shutdown()` (production: SIGTERM | SIGINT), so the accept
    // loop has a uniform branch to await.
    let shutdown_future: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        match shutdown_rx {
            Some(rx) => Box::pin(async move {
                let _ = rx.await;
            }),
            None => Box::pin(wait_for_shutdown()),
        };
    tokio::pin!(shutdown_future);

    #[cfg(unix)]
    {
        let socket_path = socket_override.unwrap_or_else(|| dir.join("logmon.sock"));
        // Remove stale socket file if it exists
        let _ = std::fs::remove_file(&socket_path);
        let listener = tokio::net::UnixListener::bind(&socket_path)?;
        info!(?socket_path, "listening on Unix socket");

        // Track spawned connection-handler tasks so we can abort them on
        // shutdown. Without this, the accept loop exits but per-connection
        // tasks remain alive — they keep serving requests on the old socket
        // after `shutdown_future` resolves, which makes restart-based
        // reconnect testing impossible.
        let mut connection_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

        // Accept loop with override-aware shutdown for graceful termination.
        loop {
            // Honor accept-pause: when paused, do not call accept(). Sleep a
            // tick and re-check. Shutdown still wins because the select! below
            // races the pause-sleep against the shutdown future.
            if let Some(p) = accept_paused.as_ref() {
                if p.load(Ordering::SeqCst) {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                            continue;
                        }
                        _ = &mut shutdown_future => {
                            info!("received shutdown signal during accept pause");
                            break;
                        }
                    }
                }
            }

            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let handler = handler.clone();
                            let pipeline = pipeline.clone();
                            let sessions = sessions.clone();
                            connection_tasks.spawn(async move {
                                if let Err(e) = handle_connection(stream, handler, pipeline, sessions).await {
                                    warn!("connection error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            error!("accept error: {e}");
                        }
                    }
                }
                // Reap finished connection tasks so the JoinSet doesn't grow
                // unbounded over the daemon's lifetime. Returns None when
                // empty, which the select macro treats as never-ready — we
                // won't busy-loop.
                Some(_) = connection_tasks.join_next() => {}
                _ = &mut shutdown_future => {
                    info!("received shutdown signal, stopping");
                    break;
                }
            }
        }

        // Abort any in-flight connection-handler tasks. This is what makes
        // shutdown actually disconnect clients (without it, per-connection
        // tasks survive the accept-loop exit).
        connection_tasks.shutdown().await;

        // Send offline beacon before stopping receivers
        send_otel_beacon("OTEL:OFFLINE\n");

        // Persist live named-session state (triggers, filters, client_info)
        // before tearing down. Best-effort: failures are logged but do not
        // block shutdown.
        let snapshot = sessions.snapshot_named_for_persistence();
        let final_state = DaemonState {
            seq_block: reserved_seq_block,
            named_sessions: snapshot,
        };
        if let Err(e) = save_state(&state_path, &final_state) {
            warn!("failed to save state on shutdown: {e}");
        }

        // Cleanup
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(&pid_path);
    }

    #[cfg(windows)]
    {
        // socket_override unused on Windows — broker listens on TCP.
        let _ = socket_override;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:12200").await?;
        info!("listening on TCP 127.0.0.1:12200");

        let mut connection_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

        loop {
            if let Some(p) = accept_paused.as_ref() {
                if p.load(Ordering::SeqCst) {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                            continue;
                        }
                        _ = &mut shutdown_future => {
                            info!("received shutdown signal during accept pause");
                            break;
                        }
                    }
                }
            }

            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            info!(?addr, "new TCP connection");
                            let handler = handler.clone();
                            let pipeline = pipeline.clone();
                            let sessions = sessions.clone();
                            connection_tasks.spawn(async move {
                                if let Err(e) = handle_connection(stream, handler, pipeline, sessions).await {
                                    warn!("connection error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            error!("accept error: {e}");
                        }
                    }
                }
                Some(_) = connection_tasks.join_next() => {}
                _ = &mut shutdown_future => {
                    info!("received shutdown signal, stopping");
                    break;
                }
            }
        }

        // Abort any in-flight connection-handler tasks so shutdown actually
        // disconnects clients.
        connection_tasks.shutdown().await;

        // Send offline beacon before stopping receivers
        send_otel_beacon("OTEL:OFFLINE\n");

        // Persist live named-session state (triggers, filters, client_info)
        // before tearing down. Best-effort: failures are logged but do not
        // block shutdown.
        let snapshot = sessions.snapshot_named_for_persistence();
        let final_state = DaemonState {
            seq_block: reserved_seq_block,
            named_sessions: snapshot,
        };
        if let Err(e) = save_state(&state_path, &final_state) {
            warn!("failed to save state on shutdown: {e}");
        }

        let _ = std::fs::remove_file(&pid_path);
    }

    Ok(())
}

/// Handle a single client connection.
async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    handler: Arc<RpcHandler>,
    pipeline: Arc<LogPipeline>,
    sessions: Arc<SessionRegistry>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    // 1. Read first request -- must be session.start
    let first_request = match read_request(&mut reader).await? {
        Some(req) => req,
        None => return Ok(()), // EOF immediately
    };

    if first_request.method != "session.start" {
        let resp = RpcResponse::error(
            first_request.id,
            -32600,
            "first request must be session.start",
        );
        write_message(&mut writer, &resp).await?;
        return Ok(());
    }

    // 2. Validate protocol version
    let params: SessionStartParams = serde_json::from_value(first_request.params.clone())
        .unwrap_or(SessionStartParams {
            name: None,
            protocol_version: 0,
            client_info: None,
        });

    if params.protocol_version != PROTOCOL_VERSION {
        let resp = RpcResponse::error(
            first_request.id,
            -32600,
            &format!(
                "unsupported protocol version: {} (expected {})",
                params.protocol_version, PROTOCOL_VERSION
            ),
        );
        write_message(&mut writer, &resp).await?;
        return Ok(());
    }

    // 2b. Validate client_info size (≤ 4 KB serialized) BEFORE creating the
    //     session, so an oversize payload doesn't pollute the registry.
    if let Some(ci) = &params.client_info {
        let serialized = serde_json::to_string(ci).unwrap_or_default();
        if serialized.len() > 4096 {
            let resp = RpcResponse::error(
                first_request.id,
                -32602,
                "client_info exceeds 4 KB limit",
            );
            write_message(&mut writer, &resp).await?;
            return Ok(());
        }
    }

    // 3. Create/reconnect session
    let (session_id, is_new) = match &params.name {
        Some(name) => {
            // Try create_named first; if it fails (already exists), try reconnect
            match sessions.create_named(name) {
                Ok(id) => (id, true),
                Err(_) => {
                    let id = SessionId::Named(name.clone());
                    match sessions.reconnect(&id) {
                        Ok(()) => (id, false),
                        Err(e) => {
                            let resp = RpcResponse::error(
                                first_request.id,
                                -32600,
                                &format!("session error: {e}"),
                            );
                            write_message(&mut writer, &resp).await?;
                            return Ok(());
                        }
                    }
                }
            }
        }
        None => {
            let id = sessions.create_anonymous();
            (id, true)
        }
    };

    // 3b. Store client_info on the session if provided. On reconnect with no
    //     client_info, prior value is preserved.
    if params.client_info.is_some() {
        sessions.set_client_info(&session_id, params.client_info.clone());
    }

    info!(?session_id, is_new, "session started");

    // 4. Send session start response
    let mut start_result = handler.build_session_start_result(&session_id);
    start_result.is_new = is_new;
    let resp = RpcResponse::success(first_request.id, serde_json::to_value(&start_result)?);
    write_message(&mut writer, &resp).await?;

    // 5. Drain queued notifications and send each as RPC notification
    let queued = sessions.drain_notifications(&session_id);
    for event in queued {
        let payload = pipeline_event_to_trigger_fired(&event)?;
        let notification = RpcNotification::new(TRIGGER_FIRED_METHOD, payload);
        write_message(&mut writer, &notification).await?;
    }

    // 6. Subscribe to pipeline events for live trigger notifications
    let mut event_rx = pipeline.subscribe_events();

    // 7. Main loop
    loop {
        tokio::select! {
            request_result = read_request(&mut reader) => {
                match request_result {
                    Ok(Some(request)) => {
                        let response = handler.handle(&session_id, &request);
                        write_message(&mut writer, &response).await?;
                    }
                    Ok(None) => {
                        // EOF
                        info!(?session_id, "client disconnected (EOF)");
                        break;
                    }
                    Err(e) => {
                        warn!(?session_id, "read error: {e}");
                        break;
                    }
                }
            }
            event_result = event_rx.recv() => {
                match event_result {
                    Ok(event) => {
                        // The broadcast carries events for ALL sessions; only
                        // forward those tagged with our session_id.
                        if event.session_id != session_id.to_string() {
                            continue;
                        }
                        let payload = pipeline_event_to_trigger_fired(&event)?;
                        let notification = RpcNotification::new(
                            TRIGGER_FIRED_METHOD,
                            payload,
                        );
                        if let Err(e) = write_message(&mut writer, &notification).await {
                            warn!(?session_id, "write error sending notification: {e}");
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(?session_id, n, "broadcast lagged, dropped events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!(?session_id, "event channel closed");
                        break;
                    }
                }
            }
        }
    }

    // Disconnect session
    sessions.disconnect(&session_id);
    info!(?session_id, "session disconnected");
    Ok(())
}
