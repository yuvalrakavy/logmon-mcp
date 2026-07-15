//! Runtime domain lifecycle — bind a `domains.create`d domain's receivers and
//! spawn its processors (Wave 2 stage 2.2b, ephemeral domains only).
//!
//! The keystone `default` domain is fed by the daemon-level receivers built in
//! [`crate::daemon::server::run_with_overrides`]; this module builds the SAME
//! machinery for an additional domain, bound to its own ports and scoped to the
//! sessions bound to it (§9.1). Binding is SYNCHRONOUS (GELF binds directly,
//! OTLP pre-binds, §9.3) so a port clash surfaces as a clean `Err` here rather
//! than a silently-dead task.

use crate::daemon::domain::{Domain, DomainConfig, DomainId, DomainReceivers, DomainSource};
use crate::daemon::log_processor::spawn_log_processor;
use crate::daemon::session::SessionRegistry;
use crate::daemon::span_processor::spawn_span_processor;
use crate::gelf::message::LogEntry;
use crate::receiver::gelf::{GelfReceiver, GelfReceiverConfig};
use crate::receiver::otlp::{OtlpReceiver, OtlpReceiverConfig};
use crate::receiver::ReceiverMetrics;
use crate::span::types::SpanEntry;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Per-domain ingest channel capacity — matches the daemon-level receiver caps
/// (`server.rs`). Receivers use `try_send`, so overflow bumps drop counters
/// rather than parking the listener.
const LOG_CHANNEL_CAP: usize = 65_536;
const SPAN_CHANNEL_CAP: usize = 65_536;

/// Requested ports for a new domain. `None` → auto-allocate a free port;
/// `Some(0)` → disable that receiver; `Some(n)` → bind exactly `n`.
#[derive(Debug, Clone, Copy)]
pub struct DomainPortSpec {
    pub gelf: Option<u16>,
    pub otlp_grpc: Option<u16>,
    pub otlp_http: Option<u16>,
}

/// Bind a fresh ephemeral domain's receivers, spawn its domain-scoped log + span
/// processors, and return the assembled `Arc<Domain>`. Does NOT insert into the
/// registry — the caller does that after its idempotency / `max_domains` checks.
///
/// The domain's `ReceiverMetrics` is shared with its receivers so drop counts
/// are visible to `status.get`. A fresh ephemeral domain starts at seq 0.
pub async fn spawn_ephemeral_domain(
    id: DomainId,
    ports: DomainPortSpec,
    log_buffer_size: usize,
    span_buffer_size: usize,
    sessions: Arc<SessionRegistry>,
) -> anyhow::Result<Arc<Domain>> {
    let (log_tx, log_rx) = mpsc::channel::<LogEntry>(LOG_CHANNEL_CAP);
    let (span_tx, span_rx) = mpsc::channel::<SpanEntry>(SPAN_CHANNEL_CAP);
    let metrics = Arc::new(ReceiverMetrics::new());

    // --- GELF (binds synchronously) ---
    let gelf = start_gelf(ports.gelf, &log_tx, &metrics).await?;
    let gelf_port = gelf.as_ref().map_or(0, |g| g.udp_port());

    // --- OTLP (pre-binds synchronously). Disabled iff BOTH arms are Some(0),
    //     mirroring the daemon gate (server.rs `otlp_grpc>0 || otlp_http>0`).
    //     Each active arm: None/Some(0) → bind `:0` (kernel-allocated),
    //     Some(n) → bind `n`. ---
    let otlp_disabled = ports.otlp_grpc == Some(0) && ports.otlp_http == Some(0);
    let otlp = if otlp_disabled {
        // No span source → drop span_tx so the span processor exits at once.
        drop(span_tx);
        None
    } else {
        let cfg = OtlpReceiverConfig {
            grpc_addr: format!("0.0.0.0:{}", ports.otlp_grpc.unwrap_or(0)),
            http_addr: format!("0.0.0.0:{}", ports.otlp_http.unwrap_or(0)),
        };
        Some(OtlpReceiver::start(cfg, log_tx.clone(), span_tx, metrics.clone()).await?)
    };
    let (otlp_grpc_port, otlp_http_port) = otlp
        .as_ref()
        .map_or((0, 0), |o| (o.grpc_port(), o.http_port()));

    // Drop our original log_tx; the receivers hold their own clones, so the log
    // channel closes (and its processor exits) only once the receivers stop.
    drop(log_tx);

    // --- Assemble the Domain (fresh seq space; metrics shared with receivers) ---
    let config = DomainConfig {
        name: id.clone(),
        gelf_port,
        otlp_grpc_port,
        otlp_http_port,
        log_buffer_size,
        span_buffer_size,
        source: DomainSource::Ephemeral,
    };
    let mut domain = Domain::new_with_metrics(config, 0, metrics);

    // --- Domain-scoped processors (serve only sessions bound to `id`, §9.1) ---
    let log_processor =
        spawn_log_processor(log_rx, domain.pipeline.clone(), sessions.clone(), id.clone());
    let span_processor = spawn_span_processor(
        span_rx,
        domain.span_store.clone(),
        sessions,
        domain.pipeline.clone(),
        id,
    );

    domain.receivers = Some(DomainReceivers::new(gelf, otlp, log_processor, span_processor));
    Ok(Arc::new(domain))
}

/// Start the GELF receiver for the requested port policy. `Some(0)` → disabled
/// (`Ok(None)`); `Some(n)` → bind exactly `n` (clean `Err` on clash); `None` →
/// allocate a free port, retrying to absorb the bind-after-free race (and the
/// rare case where a TCP-free port is UDP-held — GELF binds UDP first).
async fn start_gelf(
    port: Option<u16>,
    log_tx: &mpsc::Sender<LogEntry>,
    metrics: &Arc<ReceiverMetrics>,
) -> anyhow::Result<Option<GelfReceiver>> {
    match port {
        Some(0) => Ok(None),
        Some(p) => {
            let cfg = GelfReceiverConfig {
                udp_addr: format!("0.0.0.0:{p}"),
                tcp_addr: format!("0.0.0.0:{p}"),
            };
            Ok(Some(
                GelfReceiver::start(cfg, log_tx.clone(), metrics.clone()).await?,
            ))
        }
        None => {
            let mut last_err = None;
            for _ in 0..5 {
                let p = allocate_free_port().await?;
                let cfg = GelfReceiverConfig {
                    udp_addr: format!("0.0.0.0:{p}"),
                    tcp_addr: format!("0.0.0.0:{p}"),
                };
                match GelfReceiver::start(cfg, log_tx.clone(), metrics.clone()).await {
                    Ok(r) => return Ok(Some(r)),
                    Err(e) => last_err = Some(e),
                }
            }
            Err(last_err.unwrap_or_else(|| anyhow::anyhow!("GELF port allocation failed")))
        }
    }
}

/// Allocate a likely-free port by binding `:0` and reading it back, then
/// releasing it for the real receiver to claim. A small window exists between
/// release and re-bind; callers retry on failure.
async fn allocate_free_port() -> anyhow::Result<u16> {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}
