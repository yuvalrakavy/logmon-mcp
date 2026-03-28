use crate::daemon::log_processor::{spawn_log_processor, sync_pre_buffer_size};
use crate::daemon::persistence::{
    config_dir, load_state, save_state, DaemonConfig, DaemonState, SEQ_BLOCK_SIZE,
};
use crate::daemon::rpc_handler::RpcHandler;
use crate::daemon::session::{SessionId, SessionRegistry};
use crate::engine::pipeline::LogPipeline;
use crate::engine::seq_counter::SeqCounter;
use crate::receiver::gelf::{GelfReceiver, GelfReceiverConfig};
use crate::receiver::Receiver;
use crate::rpc::transport::{read_request, write_message};
use crate::rpc::types::*;
use crate::span::store::SpanStore;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Run the logmon daemon. This function blocks until the daemon is shut down.
pub async fn run_daemon(config: DaemonConfig) -> anyhow::Result<()> {
    // 1. Create config dir
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;

    // 2. Set up file-based tracing (daily rotation)
    let file_appender = tracing_appender::rolling::daily(&dir, "daemon.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let subscriber = tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    info!("logmon daemon starting");

    // 3. Load state, get initial_seq from seq_block
    let state_path = dir.join("state.json");
    let state = load_state(&state_path)?;
    let initial_seq = state.seq_block;

    // 4. Create pipeline with initial seq
    let pipeline = Arc::new(LogPipeline::new_with_seq(config.buffer_size, initial_seq));

    // 5. Reserve next seq block and save
    let new_state = DaemonState {
        seq_block: initial_seq + SEQ_BLOCK_SIZE,
        named_sessions: state.named_sessions.clone(),
    };
    save_state(&state_path, &new_state)?;

    // 6. Create SessionRegistry, restore named sessions from state
    let sessions = Arc::new(SessionRegistry::new());
    for (name, persisted) in &state.named_sessions {
        sessions.restore_named(name, persisted);
    }

    // 7. Start GELF receiver
    let (log_tx, log_rx) = mpsc::channel(1024);
    let udp_port = config.gelf_udp_port.unwrap_or(config.gelf_port);
    let tcp_port = config.gelf_tcp_port.unwrap_or(config.gelf_port);
    let gelf_config = GelfReceiverConfig {
        udp_addr: format!("0.0.0.0:{udp_port}"),
        tcp_addr: format!("0.0.0.0:{tcp_port}"),
    };
    let receiver = GelfReceiver::start(gelf_config, log_tx).await?;
    let receivers_info = receiver.listening_on();
    info!(?receivers_info, "GELF receiver started");

    // 8. Write PID file
    let pid_path = dir.join("daemon.pid");
    std::fs::write(&pid_path, std::process::id().to_string())?;

    // 9. Start log processor
    let _processor_handle = spawn_log_processor(log_rx, pipeline.clone(), sessions.clone());

    // 10. Sync pre-buffer size after restoring sessions
    sync_pre_buffer_size(&pipeline, &sessions);

    // 11. Create RpcHandler
    let dummy_seq = Arc::new(SeqCounter::new());
    let span_store = Arc::new(SpanStore::new(config.buffer_size, dummy_seq));
    let handler = Arc::new(RpcHandler::new(
        pipeline.clone(),
        span_store,
        sessions.clone(),
        receivers_info,
    ));

    // 12. Listen on Unix socket (unix) or TCP (windows)
    info!("daemon ready, listening for connections");

    #[cfg(unix)]
    {
        let socket_path = dir.join("logmon.sock");
        // Remove stale socket file if it exists
        let _ = std::fs::remove_file(&socket_path);
        let listener = tokio::net::UnixListener::bind(&socket_path)?;
        info!(?socket_path, "listening on Unix socket");

        // Accept loop with ctrl_c for graceful shutdown
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let handler = handler.clone();
                            let pipeline = pipeline.clone();
                            let sessions = sessions.clone();
                            tokio::spawn(async move {
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
                _ = tokio::signal::ctrl_c() => {
                    info!("received ctrl-c, shutting down");
                    break;
                }
            }
        }

        // Cleanup
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(&pid_path);
    }

    #[cfg(windows)]
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:12200").await?;
        info!("listening on TCP 127.0.0.1:12200");

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            info!(?addr, "new TCP connection");
                            let handler = handler.clone();
                            let pipeline = pipeline.clone();
                            let sessions = sessions.clone();
                            tokio::spawn(async move {
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
                _ = tokio::signal::ctrl_c() => {
                    info!("received ctrl-c, shutting down");
                    break;
                }
            }
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

    info!(?session_id, is_new, "session started");

    // 4. Send session start response
    let mut start_result = handler.build_session_start_result(&session_id);
    start_result.is_new = is_new;
    let resp = RpcResponse::success(first_request.id, serde_json::to_value(&start_result)?);
    write_message(&mut writer, &resp).await?;

    // 5. Drain queued notifications and send each as RPC notification
    let queued = sessions.drain_notifications(&session_id);
    for event in queued {
        let notification = RpcNotification::new("trigger.fired", serde_json::to_value(&event)?);
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
                        let notification = RpcNotification::new(
                            "trigger.fired",
                            serde_json::to_value(&event)?,
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
