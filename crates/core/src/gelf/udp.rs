use crate::gelf::message::{LogEntry, parse_gelf_message};
use crate::receiver::ReceiverMetrics;
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

pub async fn start_udp_listener(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
    #[allow(unused_variables)]
    metrics: Arc<ReceiverMetrics>,
) -> anyhow::Result<UdpListenerHandle> {
    let socket = UdpSocket::bind(addr).await?;
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
                            Ok(entry) => { let _ = sender.send(entry).await; }
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
