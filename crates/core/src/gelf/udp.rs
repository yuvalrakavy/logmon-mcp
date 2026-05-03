use crate::gelf::message::{parse_gelf_message, LogEntry};
use crate::receiver::{ReceiverMetrics, ReceiverSource};
use socket2::{Domain, Protocol, Socket, Type};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

pub struct UdpListenerHandle {
    port: u16,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl UdpListenerHandle {
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Target receive-buffer size for the UDP socket. macOS' default is ~256 KB,
/// which can drop datagrams under burst even when the user-space task drains
/// quickly. 8 MB matches the expected burst headroom of the user-space
/// 65 536-entry channel; the kernel may cap to its own `kern.ipc.maxsockbuf`,
/// which is fine — best-effort.
const UDP_RCVBUF_BYTES: usize = 8 * 1024 * 1024;

pub async fn start_udp_listener(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
    metrics: Arc<ReceiverMetrics>,
) -> anyhow::Result<UdpListenerHandle> {
    // Create via socket2 so we can set SO_RCVBUF before bind.
    let sock_addr: std::net::SocketAddr = addr.parse()?;
    let domain = match sock_addr {
        std::net::SocketAddr::V4(_) => Domain::IPV4,
        std::net::SocketAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if let Err(e) = socket.set_recv_buffer_size(UDP_RCVBUF_BYTES) {
        tracing::warn!(error = %e, "could not set GELF UDP SO_RCVBUF; continuing with default");
    }
    socket.set_nonblocking(true)?;
    socket.bind(&sock_addr.into())?;
    let std_socket: std::net::UdpSocket = socket.into();
    let socket = UdpSocket::from_std(std_socket)?;
    let port = socket.local_addr()?.port();
    let (tx, mut rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let mut buf = [0u8; 65535];
        loop {
            tokio::select! {
                result = socket.recv_from(&mut buf) => {
                    if let Ok((len, _addr)) = result {
                        // Strip trailing null bytes (some GELF libraries append TCP-style null delimiters to UDP)
                        let mut end = len;
                        while end > 0 && buf[end - 1] == 0 {
                            end -= 1;
                        }
                        // Parse with seq=0 — daemon assigns real seq later
                        match parse_gelf_message(&buf[..end], 0) {
                            Ok(entry) => {
                                let _ = metrics.try_send_log(&sender, entry, ReceiverSource::GelfUdp);
                            }
                            Err(e) => { eprintln!("malformed GELF UDP: {e}"); }
                        }
                    }
                }
                _ = &mut rx => break,
            }
        }
    });

    Ok(UdpListenerHandle { port, _shutdown: tx })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::ReceiverMetrics;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// Sending many GELF UDP datagrams when the channel is full must NOT
    /// park the receiver task. Counter increments instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_channel_does_not_park_udp_listener() {
        let (sender, _rx) = mpsc::channel(1);
        // Fill the channel pre-emptively.
        sender.try_send(crate::gelf::message::LogEntry {
            seq: 0, timestamp: chrono::Utc::now(),
            level: crate::gelf::message::Level::Info,
            message: "filler".into(), full_message: None,
            host: "h".into(), facility: None, file: None, line: None,
            additional_fields: std::collections::HashMap::new(),
            trace_id: None, span_id: None,
            matched_filters: vec![],
            source: crate::gelf::message::LogSource::Filter,
        }).unwrap();

        let metrics = Arc::new(ReceiverMetrics::new());
        let handle = start_udp_listener("127.0.0.1:0", sender.clone(), metrics.clone())
            .await
            .unwrap();
        let port = handle.port();

        // Send 50 well-formed GELF UDP datagrams; the listener task must
        // drain the OS buffer immediately for each one (since try_send
        // returns instantly) and bump the counter.
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let payload = br#"{"version":"1.1","host":"h","short_message":"x","level":6,"timestamp":1.0}"#;
        for _ in 0..50 {
            socket.send_to(payload, format!("127.0.0.1:{port}")).await.unwrap();
        }
        // Tiny sleep to let the listener pick up the datagrams.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // At least most of the 50 must have been counted as drops; some
        // may have been kernel-dropped before reaching us. Real assertion:
        // > 0, because the channel was already full.
        let snap = metrics.snapshot();
        assert!(snap.gelf_udp >= 1, "expected at least one user-space drop, got {:?}", snap);
    }
}
