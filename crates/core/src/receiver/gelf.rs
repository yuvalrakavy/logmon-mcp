use super::{Receiver, ReceiverMetrics};
use crate::gelf::message::LogEntry;
use crate::gelf::{tcp, udp};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct GelfReceiverConfig {
    pub udp_addr: String,
    pub tcp_addr: String,
}

pub struct GelfReceiver {
    udp_handle: udp::UdpListenerHandle,
    tcp_handle: tcp::TcpListenerHandle,
}

impl GelfReceiver {
    pub async fn start(
        config: GelfReceiverConfig,
        sender: mpsc::Sender<LogEntry>,
        metrics: Arc<ReceiverMetrics>,
    ) -> anyhow::Result<Self> {
        let udp_handle =
            udp::start_udp_listener(&config.udp_addr, sender.clone(), metrics.clone()).await?;
        let tcp_handle = tcp::start_tcp_listener(&config.tcp_addr, sender, metrics).await?;
        Ok(Self {
            udp_handle,
            tcp_handle,
        })
    }

    pub fn udp_port(&self) -> u16 {
        self.udp_handle.port()
    }

    pub fn tcp_port(&self) -> u16 {
        self.tcp_handle.port()
    }
}

#[async_trait]
impl Receiver for GelfReceiver {
    fn name(&self) -> &str {
        "gelf"
    }

    fn listening_on(&self) -> Vec<String> {
        vec![
            format!("UDP:{}", self.udp_handle.port()),
            format!("TCP:{}", self.tcp_handle.port()),
        ]
    }

    async fn shutdown(self: Box<Self>) {
        // Dropping the handles triggers their oneshot shutdown channels
        drop(self);
    }
}
