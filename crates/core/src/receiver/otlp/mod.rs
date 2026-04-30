pub mod grpc;
pub mod http;
pub mod mapping;

use crate::gelf::message::LogEntry;
use crate::receiver::Receiver;
use crate::span::types::SpanEntry;
use async_trait::async_trait;
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
    ) -> anyhow::Result<Self> {
        let (shutdown_tx, _) = broadcast::channel(1);
        let grpc_addr: std::net::SocketAddr = config.grpc_addr.parse()?;
        let http_addr: std::net::SocketAddr = config.http_addr.parse()?;

        let grpc_handle = {
            let log_tx = log_sender.clone();
            let span_tx = span_sender.clone();
            let rx = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) = grpc::start_grpc_server(grpc_addr, log_tx, span_tx, rx).await {
                    tracing::error!("OTLP gRPC server error: {e}");
                }
            })
        };

        let http_handle = {
            let rx = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) =
                    http::start_http_server(http_addr, log_sender, span_sender, rx).await
                {
                    tracing::error!("OTLP HTTP server error: {e}");
                }
            })
        };

        Ok(Self {
            shutdown_tx,
            grpc_port: grpc_addr.port(),
            http_port: http_addr.port(),
            grpc_handle,
            http_handle,
        })
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
