pub mod grpc;
pub mod http;
pub mod mapping;

use crate::gelf::message::LogEntry;
use crate::receiver::{Receiver, ReceiverMetrics};
use crate::span::types::SpanEntry;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

pub struct OtlpReceiverConfig {
    pub grpc_addr: String,
    pub http_addr: String,
}

pub struct OtlpReceiver {
    shutdown_tx: broadcast::Sender<()>,
    grpc_port: u16,
    http_port: u16,
    #[allow(dead_code)]
    grpc_handle: JoinHandle<()>,
    #[allow(dead_code)]
    http_handle: JoinHandle<()>,
}

impl OtlpReceiver {
    pub async fn start(
        config: OtlpReceiverConfig,
        log_sender: mpsc::Sender<LogEntry>,
        span_sender: mpsc::Sender<SpanEntry>,
        metrics: Arc<ReceiverMetrics>,
    ) -> anyhow::Result<Self> {
        let (shutdown_tx, _) = broadcast::channel(1);
        let grpc_addr: std::net::SocketAddr = config.grpc_addr.parse()?;
        let http_addr: std::net::SocketAddr = config.http_addr.parse()?;

        // Pre-bind BOTH listeners synchronously (§9.3). A port clash surfaces
        // here as a clean `Err` from `start()` — not a silently-dead spawned
        // task — which is what lets `domains.create` report conflict errors and
        // aligns OTLP with the GELF receiver's fail-loud-on-clash boot. Binding
        // to port 0 lets the kernel allocate; `local_addr()` reports the chosen
        // port (used when a domain's OTLP ports are auto-allocated). If the HTTP
        // bind fails after the gRPC bind succeeded, `grpc_listener` drops here
        // and releases its port before we return the error.
        let grpc_listener = tokio::net::TcpListener::bind(grpc_addr).await?;
        let http_listener = tokio::net::TcpListener::bind(http_addr).await?;
        let grpc_port = grpc_listener.local_addr()?.port();
        let http_port = http_listener.local_addr()?.port();

        let grpc_handle = {
            let log_tx = log_sender.clone();
            let span_tx = span_sender.clone();
            let metrics = metrics.clone();
            let rx = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) =
                    grpc::start_grpc_server(grpc_listener, log_tx, span_tx, metrics, rx).await
                {
                    tracing::error!("OTLP gRPC server error: {e}");
                }
            })
        };

        let http_handle = {
            let rx = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) =
                    http::start_http_server(http_listener, log_sender, span_sender, metrics, rx)
                        .await
                {
                    tracing::error!("OTLP HTTP server error: {e}");
                }
            })
        };

        Ok(Self {
            shutdown_tx,
            grpc_port,
            http_port,
            grpc_handle,
            http_handle,
        })
    }

    /// The bound gRPC port. Resolved from the live listener, so a request for
    /// port 0 reports the kernel-allocated port.
    pub fn grpc_port(&self) -> u16 {
        self.grpc_port
    }

    /// The bound HTTP port. Resolved from the live listener, so a request for
    /// port 0 reports the kernel-allocated port.
    pub fn http_port(&self) -> u16 {
        self.http_port
    }
}

#[async_trait]
impl Receiver for OtlpReceiver {
    fn name(&self) -> &str {
        "otlp"
    }

    fn listening_on(&self) -> Vec<String> {
        vec![
            format!("gRPC:{}", self.grpc_port),
            format!("HTTP:{}", self.http_port),
        ]
    }

    async fn shutdown(self: Box<Self>) {
        let _ = self.shutdown_tx.send(());
    }
}

impl Drop for OtlpReceiver {
    fn drop(&mut self) {
        // Best-effort: signal graceful shutdown. axum's
        // `with_graceful_shutdown` waits for in-flight handlers to complete
        // — which is fine for handlers that finish quickly, but if the
        // shutdown is happening because we've already wedged, in-flight
        // tasks may park on the (full) log/span channel forever. Aborting
        // the JoinHandles forces an exit path regardless. Both paths leave
        // the listening sockets to be cleaned up by the kernel after
        // process exit; the only thing we lose is graceful body
        // completion of an already-parked handler.
        let _ = self.shutdown_tx.send(());
        self.grpc_handle.abort();
        self.http_handle.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::ReceiverMetrics;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn channels() -> (
        mpsc::Sender<LogEntry>,
        mpsc::Sender<SpanEntry>,
        Arc<ReceiverMetrics>,
    ) {
        // Leak the receivers so the senders stay open for the lifetime of the
        // test (the receiver never parks on a full/closed channel here).
        let (log_tx, log_rx) = mpsc::channel(16);
        let (span_tx, span_rx) = mpsc::channel(16);
        Box::leak(Box::new((log_rx, span_rx)));
        (log_tx, span_tx, Arc::new(ReceiverMetrics::new()))
    }

    /// §9.3: a gRPC port clash must surface as a synchronous `Err` from
    /// `start()`, not a silently-dead spawned task. `domains.create` relies on
    /// this to report clean conflict errors.
    #[tokio::test]
    async fn start_errors_synchronously_on_grpc_port_clash() {
        // Squat the gRPC port so the receiver's bind must fail.
        let squatter = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken = squatter.local_addr().unwrap().port();

        let (log_tx, span_tx, metrics) = channels();
        let cfg = OtlpReceiverConfig {
            grpc_addr: format!("127.0.0.1:{taken}"),
            http_addr: "127.0.0.1:0".to_string(), // free
        };
        let result = OtlpReceiver::start(cfg, log_tx, span_tx, metrics).await;
        assert!(
            result.is_err(),
            "expected a synchronous bind error when the gRPC port is already held"
        );
    }

    /// Hazard symmetry: the same must hold for the HTTP port.
    #[tokio::test]
    async fn start_errors_synchronously_on_http_port_clash() {
        let squatter = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken = squatter.local_addr().unwrap().port();

        let (log_tx, span_tx, metrics) = channels();
        let cfg = OtlpReceiverConfig {
            grpc_addr: "127.0.0.1:0".to_string(), // free
            http_addr: format!("127.0.0.1:{taken}"),
        };
        let result = OtlpReceiver::start(cfg, log_tx, span_tx, metrics).await;
        assert!(
            result.is_err(),
            "expected a synchronous bind error when the HTTP port is already held"
        );
    }

    /// On free ports `start` succeeds and the accessors report the actually
    /// bound (kernel-allocated, since 0 was requested) ports.
    #[tokio::test]
    async fn start_succeeds_on_free_ports_and_reports_allocated_ports() {
        let (log_tx, span_tx, metrics) = channels();
        let cfg = OtlpReceiverConfig {
            grpc_addr: "127.0.0.1:0".to_string(),
            http_addr: "127.0.0.1:0".to_string(),
        };
        let recv = OtlpReceiver::start(cfg, log_tx, span_tx, metrics)
            .await
            .expect("start on free ports must succeed");
        assert_ne!(recv.grpc_port(), 0, "gRPC port should be kernel-allocated");
        assert_ne!(recv.http_port(), 0, "HTTP port should be kernel-allocated");
        assert_ne!(
            recv.grpc_port(),
            recv.http_port(),
            "gRPC and HTTP must bind distinct ports"
        );
    }
}
