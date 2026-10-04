use crate::daemon::domain::{Domain, DomainConfig, DomainId, DomainRegistry, DomainSource};
use crate::daemon::domain_lifecycle::{spawn_ephemeral_domain, DomainPortSpec};
use crate::daemon::log_processor::{spawn_log_processor, sync_pre_buffer_size};
use crate::daemon::persistence::{
    config_dir, load_state, save_state, DaemonConfig, DaemonState, SEQ_BLOCK_SIZE,
};
use crate::daemon::rpc_handler::{DomainPolicy, RpcHandler};
use crate::daemon::session::{SessionId, SessionRegistry};
use crate::daemon::span_processor::spawn_span_processor;
use crate::daemon::transport::{write_message, RequestReader};
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

/// Failed accepts on the broker's client sockets. Throttled and paced, because one that repeats
/// — out of file descriptors — came straight back on every retry: a spin at full CPU writing
/// an ERROR line per turn, while every client waited in the backlog.
static ACCEPT_ERRORS: crate::throttle::Throttle = crate::throttle::Throttle::new();

/// Notification method name for fired triggers. Underscore form matches
/// `notifications/<name>` rationalization (vs the legacy dot form).
const TRIGGER_FIRED_METHOD: &str = "trigger_fired";

/// How long a new connection has to send `session.start`. Every client sends it at once; the
/// bound is for one that never does, which otherwise held a task and a socket for as long as
/// it stayed connected.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long one message may take to write to a connection's client. A client that stopped
/// reading filled its socket, and the write then waited for as long as the client stayed
/// connected — its session marked connected, so its name was refused to anyone else. Past this
/// the connection is closed (and its session disconnected) instead. The bound is on the whole
/// message, so a client still reading, but too slowly to take one message in 30 s, is closed
/// too; every client here reads on a task of its own, over a local socket.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// [`write_message`] bounded by [`WRITE_TIMEOUT`] — every write a connection makes goes
/// through here.
async fn write_bounded<W: tokio::io::AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &impl serde::Serialize,
) -> anyhow::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, write_message(writer, msg))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "a message to the client did not finish writing within {WRITE_TIMEOUT:?}"
            )
        })?
}

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
fn send_otel_beacon(message: &str, target: Option<std::net::SocketAddr>) {
    use std::net::UdpSocket;
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        let _ = match target {
            Some(addr) => socket.send_to(message.as_bytes(), addr),
            None => socket.send_to(message.as_bytes(), "239.255.77.1:4399"),
        };
    }
}

/// Announce `OTEL:ONLINE` — once the broker can serve, i.e. after its listener is bound, the
/// last fallible step of startup. Only a broker whose OTLP receiver started announces anything,
/// and only that one owes `OTEL:OFFLINE` at shutdown (the same `_otlp_receiver.is_some()` gates
/// both). Default-domain ONLY, by design (consumer #4/§18): the beacon carries no domain/port,
/// so non-default domains never emit it (see `domain_lifecycle`, which sends none); a producer
/// targeting a non-default domain uses `create_domain` returning (its OTLP port pre-binds
/// synchronously) as the readiness signal.
fn announce_online(otlp_started: bool, target: Option<std::net::SocketAddr>) {
    if otlp_started {
        send_otel_beacon("OTEL:ONLINE\n", target);
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
    /// If `Some`, send the OTEL availability beacons (`ONLINE`/`OFFLINE`) here instead of
    /// the host-wide multicast group. The beacon carries no port, so every tracing-init
    /// producer on the host acts on it: a test daemon that multicast `OFFLINE` silenced
    /// the LIVE broker's producers for the reprobe interval. The test harness points this
    /// at a socket of its own, which also lets a test read what a daemon announced.
    pub beacon_target: Option<std::net::SocketAddr>,
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
        beacon_target,
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

    // Every error from here on is logged before it propagates, so it lands in `daemon.log`
    // (gh #28). A service manager restarts a broker that exits, and under launchd stderr went
    // nowhere, so a broker that could not start — the GELF port taken, another broker already
    // running, an unreadable state file — restart-looped with no trace of why. A failure BEFORE
    // this point (the config dir, tracing itself, `load_config` in `main`) still has only
    // stderr, which the service definitions now capture (`daemon.stderr.log`).
    let result = run_initialized(
        config,
        dir,
        socket_override,
        injected_log_rx,
        shutdown_rx,
        accept_paused,
        beacon_target,
    )
    .await;
    if let Err(e) = &result {
        error!("logmon daemon failed: {e:#}");
    }
    result
}

/// [`run_with_overrides`] from the config dir and tracing onward: everything that can fail
/// once there is a log to say so in.
async fn run_initialized(
    config: DaemonConfig,
    dir: PathBuf,
    socket_override: Option<PathBuf>,
    injected_log_rx: Option<mpsc::Receiver<LogEntry>>,
    shutdown_rx: Option<oneshot::Receiver<()>>,
    accept_paused: Option<Arc<AtomicBool>>,
    beacon_target: Option<std::net::SocketAddr>,
) -> anyhow::Result<()> {
    // 2a. Refuse a buffer size no domain could allocate — it would otherwise abort the process
    //     on that domain's first record. After tracing (which binds nothing), so the refusal
    //     is logged by the caller; still before anything binds or allocates.
    config.validate_buffer_sizes()?;

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

    // 2c. Temp-file sweep. Every durable write goes temp → fsync → rename, so
    //     a crash between the write and the rename leaves the temp behind.
    //     The pid sweep above clears only `daemon.pid` and the socket, so
    //     without this each crash leaks a file forever.
    //     Both the config dir AND the collectors subdirectory: `atomic_write`
    //     puts its temp beside its target, and collector files live one level
    //     down, so sweeping only the top level misses exactly the files this
    //     feature added. `load_all` will not reclaim them either — it filters
    //     on a `.json` extension and a temp ends in `.tmp-write`.
    let swept = crate::daemon::persistence::sweep_temp_files(&dir)
        + crate::daemon::persistence::sweep_temp_files(
            &dir.join(crate::collector::persist::COLLECTORS_DIR),
        );
    if swept > 0 {
        info!(
            count = swept,
            "removed temp files left by an interrupted write"
        );
    }

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

    // 7. Create SessionRegistry + BookmarkStore, restore named sessions from state.
    //    BookmarkStore is created here (rather than later) so persisted
    //    bookmarks can be hydrated alongside triggers/filters during restore.
    let sessions = Arc::new(SessionRegistry::new());
    // Daemon-wide, domain-keyed collector registry (spec section 4.4). One
    // instance shared by every span processor; entries carry their own pinned
    // domain, so a collector is never reached via its owner's current binding.
    // Persistence goes into the same directory the rest of the daemon's state
    // does, under a `collectors/` subdirectory so the boot sweeps for
    // `daemon.pid` and the socket cannot reach these files by accident.
    let collectors = Arc::new(
        crate::collector::registry::CollectorRegistry::new().with_persistence(dir.clone()),
    );
    let bookmark_store = Arc::new(crate::store::bookmarks::BookmarkStore::new());
    for (name, persisted) in &state.named_sessions {
        sessions.restore_named(name, persisted, &bookmark_store);
    }

    // Receiver metrics (drop counters + rate-limited warn). Shared between
    // every receiver call site and the RpcHandler so status.get can surface
    // the counts.
    let receiver_metrics = std::sync::Arc::new(crate::receiver::ReceiverMetrics::new());

    // 8/9. Receivers — either inject a pre-built channel (test harness) or
    //      start the real GELF + OTLP receivers. Both paths spawn a span
    //      processor so the wiring is uniform; in the harness path nothing
    //      pushes into the span channel so the processor sits idle.
    //
    // NOTE on receiver lifetime: BOTH `_gelf_receiver` AND `_otlp_receiver`
    // must be bound to locals that outlive the accept loop. Their `Drop`
    // impls cancel the spawned listener tasks and release the bound sockets.
    // A previous version of this code dropped `gelf_receiver` at the end of
    // the match arm, which left the daemon claiming "GELF receiver started"
    // in the log while no GELF port was actually bound — silent ingestion
    // failure. Keep both handles alive at function scope.
    let (log_rx, _gelf_receiver, _otlp_receiver, all_receivers_info) = match injected_log_rx {
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
                collectors.clone(),
                DomainId::default_domain(),
            );
            (rx, None, None, Vec::<String>::new())
        }
        None => {
            // Real GELF receiver
            // Bursts of GELF + OTLP traffic from store-test test runs can
            // briefly overshoot the consumer; 65 536 entries × ~500 B ≈ 32 MB
            // worst-case headroom keeps drop-counting from engaging on
            // realistic workloads. Receivers use try_send (see ReceiverMetrics)
            // so they never park if this cap is exceeded.
            const LOG_CHANNEL_CAP: usize = 65_536;
            const SPAN_CHANNEL_CAP: usize = 65_536;
            let (log_tx, log_rx) = mpsc::channel(LOG_CHANNEL_CAP);
            let (span_tx, span_rx_real) = mpsc::channel(SPAN_CHANNEL_CAP);
            let udp_port = config.gelf_udp_port.unwrap_or(config.gelf_port);
            let tcp_port = config.gelf_tcp_port.unwrap_or(config.gelf_port);
            let gelf_config = GelfReceiverConfig {
                udp_addr: format!("0.0.0.0:{udp_port}"),
                tcp_addr: format!("0.0.0.0:{tcp_port}"),
            };
            let gelf_receiver =
                GelfReceiver::start(gelf_config, log_tx.clone(), receiver_metrics.clone()).await?;
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
                // OTLP is an OPTIONAL receiver. A port clash at boot must NOT take
                // the daemon down (and with it GELF/logging) — degrade loudly and
                // keep serving. GELF above still fails loud (it is the core
                // function), and explicit `domains.create` still fails loud because
                // it propagates `start`'s error to the caller. Deep-gate finding C.
                match OtlpReceiver::start(
                    otlp_config,
                    log_tx.clone(),
                    span_tx,
                    receiver_metrics.clone(),
                )
                .await
                {
                    Ok(otlp_receiver) => {
                        let otlp_info = otlp_receiver.listening_on();
                        info!(?otlp_info, "OTLP receiver started");
                        all_receivers_info.extend(otlp_info);
                        // `OTEL:ONLINE` is NOT sent here: startup can still fail after this
                        // point (the pid file, the socket bind), and a broker that announced
                        // itself and then exited never says `OFFLINE` (gh #27). It goes out
                        // once the listener is bound — see `announce_online`.
                        Some(otlp_receiver)
                    }
                    Err(e) => {
                        // `span_tx` was moved into `start` and dropped on its error
                        // path, so the span processor's channel closes as in the
                        // OTLP-disabled branch below — no separate drop needed.
                        warn!(
                            grpc_port = config.otlp_grpc_port,
                            http_port = config.otlp_http_port,
                            error = %e,
                            "OTLP receiver failed to bind; continuing with OTLP DISABLED \
                             (GELF/logging unaffected). Free the port(s) or set them to 0 to silence this."
                        );
                        None
                    }
                }
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
                collectors.clone(),
                DomainId::default_domain(),
            );
            (
                log_rx,
                Some(gelf_receiver),
                otlp_receiver,
                all_receivers_info,
            )
        }
    };

    // 10. Write PID file (path was resolved earlier in step 1b for the
    //     stale-pid sweep)
    std::fs::write(&pid_path, std::process::id().to_string())?;

    // 11. Start log processor
    let _processor_handle = spawn_log_processor(
        log_rx,
        pipeline.clone(),
        sessions.clone(),
        DomainId::default_domain(),
    );

    // 12. Sync pre-buffer size after restoring sessions
    sync_pre_buffer_size(&pipeline, &sessions);

    // 12b. Assemble the `default` domain (Model A, N=1). Its machinery was
    //      built above — seeded from `state.json`'s seq_block and already used
    //      to restore named sessions — so we wrap the existing Arcs via
    //      `from_parts` rather than allocating fresh. `default` is a
    //      config-declared domain (`domains.delete` refuses it). Runtime
    //      `domains.create`/`delete` and additional domains arrive in a later
    //      stage; for now the registry holds exactly this one.
    let domains = Arc::new(DomainRegistry::new());
    domains.insert(Arc::new(Domain::from_parts(
        DomainConfig {
            name: DomainId::default_domain(),
            gelf_port: config.gelf_port,
            otlp_grpc_port: config.otlp_grpc_port,
            otlp_http_port: config.otlp_http_port,
            log_buffer_size: config.buffer_size,
            span_buffer_size: config.span_buffer_size,
            source: DomainSource::Config,
        },
        pipeline.clone(),
        span_store.clone(),
        bookmark_store.clone(),
        receiver_metrics.clone(),
    )));

    // 12c. Build config-declared domains (§17.9): user-declared durable domains
    //      re-created at boot, DECLARATIONS-ONLY (empty buffers, fresh seq — data
    //      never persists). Each binds its own receivers. A `default`-named entry,
    //      a duplicate, an invalid name, or a bind failure (port clash) is SKIPPED
    //      with a WARN so one bad entry can't take the daemon down; bound ports are
    //      logged. (Per-domain seq/bookmark durability + `persist=true` are §17
    //      deferred work.)
    let mut seen_config_domains: std::collections::HashSet<DomainId> =
        std::collections::HashSet::new();
    for cd in &config.domains {
        let id = match DomainId::new(&cd.name) {
            Ok(id) => id,
            Err(e) => {
                warn!(name = %cd.name, error = %e, "invalid config domain name; skipping");
                continue;
            }
        };
        if id == DomainId::default_domain() {
            warn!("config domain named 'default' is reserved; skipping");
            continue;
        }
        let log_sz = cd.log_buffer_size.unwrap_or(config.buffer_size);
        let span_sz = cd.span_buffer_size.unwrap_or(config.span_buffer_size);
        // A size no ring could reserve would abort the whole process on this domain's first
        // record; like any other bad entry, the domain is skipped and the daemon starts.
        let max = crate::daemon::persistence::MAX_BUFFER_SIZE;
        if log_sz > max || span_sz > max {
            warn!(
                name = %cd.name,
                log_buffer_size = log_sz,
                span_buffer_size = span_sz,
                max,
                "config domain buffer size exceeds the maximum; skipping"
            );
            continue;
        }
        // A name is claimed only by an entry that STARTS (below), so a skipped entry — too
        // large, or failed to bind — never blocks a corrected one of the same name later.
        if seen_config_domains.contains(&id) {
            warn!(name = %cd.name, "duplicate config domain name; skipping");
            continue;
        }
        let ports = DomainPortSpec {
            gelf: cd.gelf_port,
            otlp_grpc: cd.otlp_grpc_port,
            otlp_http: cd.otlp_http_port,
        };
        match spawn_ephemeral_domain(
            id.clone(),
            ports,
            log_sz,
            span_sz,
            sessions.clone(),
            collectors.clone(),
            DomainSource::Config,
        )
        .await
        {
            Ok(domain) => {
                info!(
                    name = %id,
                    gelf = domain.config.gelf_port,
                    otlp_grpc = domain.config.otlp_grpc_port,
                    otlp_http = domain.config.otlp_http_port,
                    "config domain started"
                );
                seen_config_domains.insert(id.clone());
                domains.insert(domain);
            }
            Err(e) => {
                warn!(name = %id, error = %e, "config domain failed to start (e.g. port clash); skipping");
            }
        }
    }

    // 13. Create RpcHandler. It resolves each request's bound domain out of the
    //     registry (once, at the boundary) and runs the store code against it.
    //     The domain policy (max_domains + default buffer sizes) drives
    //     `domains.create`.
    let handler = Arc::new(
        RpcHandler::new(
            domains.clone(),
            sessions.clone(),
            collectors.clone(),
            all_receivers_info,
            DomainPolicy {
                max_domains: config.max_domains,
                default_log_buffer_size: config.buffer_size,
                default_span_buffer_size: config.span_buffer_size,
                stale_after_secs: config.stale_after_secs,
            },
        )
        // 13a1. The provenance registry (case-documents spec §3). Registries open
        //       lazily per domain on first use — the skill recommends one domain
        //       per test run, so opening eagerly would leave a permanent file for
        //       every run that never recorded anything.
        .with_domain_data(Arc::new(crate::domain_data::DomainDataStore::new(
            dir.clone(),
            env!("CARGO_PKG_VERSION").to_string(),
        ))),
    );

    // 13a2. Restore collectors (§10). Deliberately AFTER the domain registry
    //       exists, so a restored collector can be handed the metrics of the
    //       domain it was pinned to. The ORPHAN check stays lazy regardless —
    //       config-declared domains are built above but an API-created one
    //       never comes back, and marking that at boot would report a failure
    //       the caller cannot yet see the context for.
    {
        let domains_for_restore = domains.clone();
        let fallback_metrics = receiver_metrics.clone();
        let report = collectors.restore(chrono::Utc::now(), move |id| {
            domains_for_restore
                .get(id)
                .map(|d| d.metrics.clone())
                // A collector whose domain is gone gets a private counter set
                // it shares with nobody. Its ingest figures are then trivially
                // zero, which is correct: no span can reach it, so none is
                // lost on its behalf either.
                .unwrap_or_else(|| fallback_metrics.clone())
        });
        if !report.restored.is_empty() {
            info!(
                collectors = ?report.restored,
                "restored collectors, armed but zeroed (a live window does not survive a restart)"
            );
        }
        // A collector's owner must be a session the daemon can still reach.
        // Collector files are write-through on `add`, but a named session only
        // reaches `state.json` on graceful shutdown — so after a `kill -9` the
        // collectors come back and their owner does not, leaving them invisible
        // to an owner-scoped `collectors.list`, unreachable by `sessions.drop`,
        // and untouched by the TTL sweep, while still holding their share of a
        // reservation that only four collectors fit inside. Registering the
        // owner as a disconnected named session puts them back inside the
        // normal lifecycle, so the existing sweep reclaims them.
        for owner in collectors.owners() {
            if let SessionId::Named(name) = &owner {
                if sessions.get(&owner).is_none() {
                    sessions.restore_named(
                        name,
                        &crate::daemon::persistence::PersistedSession {
                            triggers: Vec::new(),
                            filters: Vec::new(),
                            client_info: None,
                            bookmarks: Vec::new(),
                        },
                        &bookmark_store,
                    );
                    warn!(
                        session = %name,
                        "re-registered a session that owns collectors but was not in \
                         state.json (its last shutdown was not graceful); its collectors \
                         are reachable and sweepable again, its filters and triggers are lost"
                    );
                }
            }
        }
        for (path, reason) in &report.quarantined {
            warn!(?path, %reason, "collector file moved aside; it could not be read");
        }
        for (path, reason) in &report.superseded {
            warn!(?path, %reason,
                "collector file set aside: another copy of the same collector was newer");
        }
        for (name, reason) in &report.rejected {
            warn!(%name, %reason, "collector file describes something this build cannot arm");
        }
    }

    // 13b. Session TTL sweep (meaningful-session-names spec, 2026-07-17):
    //      a session DISCONNECTED longer than `session_ttl_secs` is disposed —
    //      the registry entry, and under the same lock its bookmarks in every
    //      domain and its collectors. Re-decided per session at disposal: one
    //      that reconnected after the listing, or reconnected and left again,
    //      is no longer abandoned and is left alone. Connected sessions never
    //      expire (TTL measures abandonment, not lifetime). Keeps
    //      unique-per-conversation names from accumulating.
    //      Wake: the interval tick. Sleep: `interval.tick().await`.
    {
        let sessions = sessions.clone();
        let handler = handler.clone();
        let ttl = std::time::Duration::from_secs(config.session_ttl_secs.max(1));
        tokio::spawn(async move {
            // Sweep at TTL/10, clamped to [60s, 1h] — timely without churn.
            let period = (ttl / 10).clamp(
                std::time::Duration::from_secs(60),
                std::time::Duration::from_secs(3600),
            );
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                for id in sessions.expired_disconnected(ttl) {
                    use crate::daemon::rpc_handler::ExpiredDisposal;
                    match handler.dispose_expired_session(&id, ttl) {
                        ExpiredDisposal::Disposed {
                            bookmarks,
                            collectors,
                        } => {
                            info!(session = %id, bookmarks_cleared = bookmarks,
                                collectors_released = collectors,
                                "session TTL sweep: disposed (disconnected past TTL)");
                        }
                        ExpiredDisposal::NotAbandoned => {
                            info!(session = %id,
                                "session TTL sweep: kept (active again since it was listed)");
                        }
                        // Dropped or displaced since the listing: nothing happened, nothing to say.
                        ExpiredDisposal::Gone => {}
                    }
                }
            }
        });
    }

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
        announce_online(_otlp_receiver.is_some(), beacon_target);

        // Track spawned connection-handler tasks so we can abort them on
        // shutdown. Without this, the accept loop exits but per-connection
        // tasks remain alive — they keep serving requests on the old socket
        // after `shutdown_future` resolves, which makes restart-based
        // reconnect testing impossible.
        let mut connection_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let mut accept_failures_in_a_row = 0u32;

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
                            accept_failures_in_a_row = 0;
                            let handler = handler.clone();
                            let domains = domains.clone();
                            let sessions = sessions.clone();
                            connection_tasks.spawn(async move {
                                if let Err(e) = handle_connection(stream, handler, domains, sessions).await {
                                    warn!("connection error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            crate::throttle::pace_after_error(
                                &ACCEPT_ERRORS,
                                &mut accept_failures_in_a_row,
                                |n| error!("accept error ({n} so far): {e}"),
                            )
                            .await;
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

        // Send the offline beacon before stopping receivers — only if this daemon announced
        // itself ONLINE, i.e. its OTLP receiver started. The beacon is a host-wide multicast
        // that opens every tracing-init producer's circuit breaker; a daemon that never
        // announced (an in-process test daemon on an injected channel, OTLP disabled or
        // failed to bind) sending it silenced the LIVE broker's producers for the reprobe
        // interval, every time a test daemon shut down.
        if _otlp_receiver.is_some() {
            send_otel_beacon("OTEL:OFFLINE\n", beacon_target);
        }

        // Persist live named-session state (triggers, filters, client_info,
        // bookmarks) before tearing down. Best-effort: failures are logged but
        // do not block shutdown.
        let snapshot = sessions.snapshot_named_for_persistence(&bookmark_store);
        // Seq-block high-water fix (§8): persist the GREATER of the reserved
        // block and the live counter. A run emitting >SEQ_BLOCK_SIZE records
        // advances the counter past the boot-time reservation; persisting only
        // the reservation would let the next boot re-hand-out those seqs and
        // alias any persisted cursor. `max(...)` guarantees seqs never rewind.
        let final_seq_block = reserved_seq_block.max(seq_counter.current());
        let final_state = DaemonState {
            seq_block: final_seq_block,
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
        announce_online(_otlp_receiver.is_some(), beacon_target);

        let mut connection_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let mut accept_failures_in_a_row = 0u32;

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
                            accept_failures_in_a_row = 0;
                            info!(?addr, "new TCP connection");
                            let handler = handler.clone();
                            let domains = domains.clone();
                            let sessions = sessions.clone();
                            connection_tasks.spawn(async move {
                                if let Err(e) = handle_connection(stream, handler, domains, sessions).await {
                                    warn!("connection error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            crate::throttle::pace_after_error(
                                &ACCEPT_ERRORS,
                                &mut accept_failures_in_a_row,
                                |n| error!("accept error ({n} so far): {e}"),
                            )
                            .await;
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

        // Send the offline beacon before stopping receivers — only if this daemon announced
        // itself ONLINE, i.e. its OTLP receiver started. The beacon is a host-wide multicast
        // that opens every tracing-init producer's circuit breaker; a daemon that never
        // announced (an in-process test daemon on an injected channel, OTLP disabled or
        // failed to bind) sending it silenced the LIVE broker's producers for the reprobe
        // interval, every time a test daemon shut down.
        if _otlp_receiver.is_some() {
            send_otel_beacon("OTEL:OFFLINE\n", beacon_target);
        }

        // Persist live named-session state (triggers, filters, client_info,
        // bookmarks) before tearing down. Best-effort: failures are logged but
        // do not block shutdown.
        let snapshot = sessions.snapshot_named_for_persistence(&bookmark_store);
        // Seq-block high-water fix (§8): persist the GREATER of the reserved
        // block and the live counter. A run emitting >SEQ_BLOCK_SIZE records
        // advances the counter past the boot-time reservation; persisting only
        // the reservation would let the next boot re-hand-out those seqs and
        // alias any persisted cursor. `max(...)` guarantees seqs never rewind.
        let final_seq_block = reserved_seq_block.max(seq_counter.current());
        let final_state = DaemonState {
            seq_block: final_seq_block,
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
    domains: Arc<DomainRegistry>,
    sessions: Arc<SessionRegistry>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    // One reader for the whole connection, the handshake included: it caps a request line
    // (`MAX_REQUEST_BYTES`), and the first line used to be read without one — any local client
    // could stream bytes with no newline until the daemon ran out of memory, before a session
    // even existed.
    let mut requests = RequestReader::default();

    // 1. Read first request -- must be session.start. Bounded: a client that connects and never
    //    sends it held this task and its socket for as long as it stayed connected.
    let first_request =
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, requests.next(&mut reader)).await {
            Ok(read) => match read? {
                Some(req) => req,
                None => return Ok(()), // EOF immediately
            },
            Err(_) => {
                warn!(
                    timeout = ?HANDSHAKE_TIMEOUT,
                    "a connection sent no session.start in time; closing it"
                );
                return Ok(());
            }
        };

    if first_request.method != "session.start" {
        let resp = RpcResponse::error(
            first_request.id,
            -32600,
            "first request must be session.start",
        );
        write_bounded(&mut writer, &resp).await?;
        return Ok(());
    }

    // 2. Validate protocol version
    let params: SessionStartParams = serde_json::from_value(first_request.params.clone())
        .unwrap_or(SessionStartParams {
            name: None,
            protocol_version: 0,
            client_info: None,
            domain: None,
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
        write_bounded(&mut writer, &resp).await?;
        return Ok(());
    }

    // 2b. Validate client_info size (≤ 4 KB serialized) BEFORE creating the
    //     session, so an oversize payload doesn't pollute the registry.
    if let Some(ci) = &params.client_info {
        let serialized = serde_json::to_string(ci).unwrap_or_default();
        if serialized.len() > 4096 {
            let resp =
                RpcResponse::error(first_request.id, -32602, "client_info exceeds 4 KB limit");
            write_bounded(&mut writer, &resp).await?;
            return Ok(());
        }
    }

    // 2c. Validate the optional connect-time domain bind BEFORE creating the
    //     session, so an unknown domain errors the handshake rather than
    //     silently leaving the session on `default`.
    let connect_domain_id = match &params.domain {
        Some(name) => match DomainId::new(name) {
            Ok(id) if domains.contains(&id) => Some(id),
            Ok(id) => {
                let resp = RpcResponse::error(
                    first_request.id,
                    -32602,
                    &format!("domain \"{id}\" does not exist — create it first"),
                );
                write_bounded(&mut writer, &resp).await?;
                return Ok(());
            }
            Err(e) => {
                let resp = RpcResponse::error(
                    first_request.id,
                    -32602,
                    &format!("invalid domain name: {e}"),
                );
                write_bounded(&mut writer, &resp).await?;
                return Ok(());
            }
        },
        None => None,
    };

    // 3. Create/reconnect session
    let (mut session_id, is_new) = match &params.name {
        // Create or take over, in one step (`claim_named` says why).
        Some(name) => match sessions.claim_named(name) {
            Ok(claimed) => claimed,
            Err(e) => {
                let resp =
                    RpcResponse::error(first_request.id, -32600, &format!("session error: {e}"));
                write_bounded(&mut writer, &resp).await?;
                return Ok(());
            }
        },
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

    // 3c. Apply the validated connect-time domain bind. Done before the event
    //     subscription resolves the session's domain (F6), so event_rx starts
    //     on the bound domain's channel.
    if let Some(id) = connect_domain_id {
        sessions.set_domain(&session_id, id);
    }

    // From here on the connection owes its session a disconnect, on EVERY exit. The writes
    // below return through `?` when the client has gone; with the cleanup at the end of the
    // function they skipped it, leaving a named session `connected` — its name refused until a
    // broker restart — and an anonymous one never removed, its triggers sizing the pre-trigger
    // buffer for good. A guard runs it on `?`, `return` and panic alike.
    let mut cleanup = SessionCleanup {
        handler: handler.clone(),
        sessions: sessions.clone(),
        session_id: session_id.clone(),
    };

    // The session's own triggers now size its domain's pre-trigger buffer. Nothing re-derived
    // it here before: a session whose default `pre_window` exceeded the domain's current size
    // — after the last session left and the buffer shrank to 0, say — got a short pre-window
    // until some unrelated trigger or filter change resynced it.
    handler.resync_pre_buffers();

    info!(?session_id, is_new, "session started");

    // 4. Send session start response
    let mut start_result = handler.build_session_start_result(&session_id);
    start_result.is_new = is_new;
    let resp = RpcResponse::success(first_request.id, serde_json::to_value(&start_result)?);
    write_bounded(&mut writer, &resp).await?;

    // 5. Drain queued notifications and send each as RPC notification
    let queued = sessions.drain_notifications(&session_id);
    for event in queued {
        let payload = pipeline_event_to_trigger_fired(&event)?;
        let notification = RpcNotification::new(TRIGGER_FIRED_METHOD, payload);
        write_bounded(&mut writer, &notification).await?;
    }

    // 6. Subscribe to the CONNECT-TIME domain's pipeline events for live
    //    trigger notifications (F6). A reconnecting named session may already
    //    be bound to a non-default domain; resolve it rather than assume. If
    //    the bound domain has vanished, fall back to `default` (always present)
    //    — the handler still surfaces the vanished-domain error on data
    //    requests. (Re-subscribe on `use_domain` arrives with binding, §9.4.)
    let connect_domain = sessions.domain_of(&session_id);
    let mut event_rx = domains
        .get(&connect_domain)
        .or_else(|| domains.get(&DomainId::default_domain()))
        .expect("default domain is always present")
        .pipeline
        .subscribe_events();
    // The domain whose event channel `event_rx` is currently subscribed to.
    // Tracked so the loop can re-subscribe when the binding changes (§9.4).
    let mut current_domain = connect_domain;

    // 7. Main loop. Requests are read through the connection's `RequestReader`: the read races
    //    the notification branch, and a request half-received when a notification wins must
    //    survive into the next iteration.
    loop {
        tokio::select! {
            request_result = requests.next(&mut reader) => {
                match request_result {
                    Ok(Some(request)) => {
                        let response = handler.handle_async(&session_id, &request).await;
                        // A successful `sessions.rename` re-keyed the registry
                        // entry; this connection must address the session by
                        // its NEW id from here on (event filtering, domain
                        // lookups, disconnect handling all key off it). BEFORE
                        // the reply is written: the rename has already
                        // happened, and a write that fails returns through
                        // `?` — the cleanup would then disconnect the old id,
                        // which no longer exists, and leave the renamed
                        // session connected for good.
                        if request.method == "sessions.rename" {
                            if let Some(new_name) = response
                                .result
                                .as_ref()
                                .and_then(|r| r.get("name"))
                                .and_then(|v| v.as_str())
                            {
                                session_id = SessionId::Named(new_name.to_string());
                                cleanup.session_id = session_id.clone();
                            }
                        }
                        write_bounded(&mut writer, &response).await?;
                        // §9.4: if the request rebound this session (domains.use),
                        // re-point the event subscription at the newly-bound
                        // domain's channel so live trigger notifications follow
                        // the rebind. F1 already cleared any stale queued events.
                        let bound = sessions.domain_of(&session_id);
                        if bound != current_domain {
                            if let Some(d) = domains.get(&bound) {
                                event_rx = d.pipeline.subscribe_events();
                                current_domain = bound;
                            }
                        }
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
                        if let Err(e) = write_bounded(&mut writer, &notification).await {
                            warn!(?session_id, "write error sending notification: {e}");
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(?session_id, n, "broadcast lagged, dropped events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // The subscribed domain's pipeline was torn down — the
                        // domain was deleted while this session was bound to it.
                        // Keep the connection ALIVE by re-subscribing to the
                        // session's current binding (or `default` if that is also
                        // gone) so the client can `domains.use` to rebind rather
                        // than being disconnected. Queries meanwhile surface the
                        // vanished-domain error (§5). `default` is never deletable,
                        // so this cannot busy-loop.
                        let bound = sessions.domain_of(&session_id);
                        if let Some(d) = domains.get(&bound) {
                            event_rx = d.pipeline.subscribe_events();
                            current_domain = bound;
                        } else if let Some(d) = domains.get(&DomainId::default_domain()) {
                            info!(?session_id, "bound domain deleted; event channel re-subscribed to default");
                            event_rx = d.pipeline.subscribe_events();
                            current_domain = DomainId::default_domain();
                        } else {
                            info!(?session_id, "event channel closed; no domain to re-subscribe");
                            break;
                        }
                    }
                }
            }
        }
    }

    // `cleanup` disconnects the session as it drops.
    Ok(())
}

/// What a connection owes its session when it ends, run on every exit — see where
/// `handle_connection` creates it.
struct SessionCleanup {
    handler: Arc<RpcHandler>,
    sessions: Arc<SessionRegistry>,
    /// The session's CURRENT id: a `sessions.rename` re-keys it.
    session_id: SessionId,
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        // Unwinding from a handler panic: a lock that panic poisoned would make the cleanup
        // panic too, and a panic during unwinding aborts the whole process. Tokio contains a
        // connection's panic to that connection; run the cleanup on a thread of its own so a
        // poisoned lock fails that thread, not the daemon.
        // (A plain function on that thread, not another guard: a guard's own `Drop` would run
        // the cleanup twice, and re-spawn without end if it panicked there.) Spawned through
        // `Builder`, which reports a refused thread as an error: `thread::spawn` panics on one,
        // and that panic, here, would be the abort this branch exists to avoid. A poisoned lock
        // still fails the cleanup on that thread — this keeps the daemon up, it does not make
        // the cleanup succeed.
        if std::thread::panicking() {
            let (handler, sessions, session_id) = (
                self.handler.clone(),
                self.sessions.clone(),
                self.session_id.clone(),
            );
            let spawned = std::thread::Builder::new()
                .name("session-cleanup".into())
                .spawn(move || disconnect_session(&handler, &sessions, &session_id));
            if let Err(e) = spawned {
                error!(session = %self.session_id,
                    "could not start the cleanup of a panicked connection: {e}");
            }
            return;
        }
        disconnect_session(&self.handler, &self.sessions, &self.session_id);
    }
}

/// [`SessionCleanup`]'s body.
fn disconnect_session(handler: &RpcHandler, sessions: &SessionRegistry, session_id: &SessionId) {
    let anonymous = matches!(session_id, SessionId::Anonymous(_));
    // Drop bookmarks for anonymous sessions; named sessions keep theirs (persisted via
    // snapshot). Collectors follow the same rule and for the same reason: an anonymous
    // session cannot be reconnected to, so anything it armed is unreachable and its share
    // of the sample reservation is pure leak.
    if anonymous {
        let removed = handler.clear_session_bookmarks(session_id);
        let collectors = handler.clear_session_collectors(session_id);
        if removed > 0 || collectors > 0 {
            info!(
                ?session_id,
                removed,
                collectors,
                "cleared anonymous-session bookmarks and collectors on disconnect"
            );
        }
    }
    sessions.disconnect(session_id);
    // An anonymous session is REMOVED on disconnect, with its triggers (a named one is kept).
    if anonymous {
        handler.resync_pre_buffers();
    }
    info!(?session_id, "session disconnected");
}
