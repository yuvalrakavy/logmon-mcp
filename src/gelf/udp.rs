use crate::engine::pipeline::LogPipeline;
use crate::gelf::message::parse_gelf_message;
use std::sync::Arc;
use tokio::net::UdpSocket;

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
    pipeline: Arc<LogPipeline>,
) -> anyhow::Result<UdpListenerHandle> {
    let socket = UdpSocket::bind(addr).await?;
    let port = socket.local_addr()?.port();
    let (tx, mut rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let mut buf = [0u8; 65535];
        loop {
            tokio::select! {
                result = socket.recv_from(&mut buf) => {
                    if let Ok((len, addr)) = result {
                        let seq = pipeline.assign_seq();
                        // Strip trailing null bytes (some GELF libraries append TCP-style null delimiters to UDP)
                        let mut end = len;
                        while end > 0 && buf[end - 1] == 0 {
                            end -= 1;
                        }
                        match parse_gelf_message(&buf[..end], seq) {
                            Ok(entry) => { pipeline.process(entry); }
                            Err(e) => {
                                let preview = String::from_utf8_lossy(&buf[..len.min(300)]).to_string();
                                pipeline.record_malformed(format!("UDP from {addr}: {e}"), preview);
                            }
                        }
                    }
                }
                _ = &mut rx => break,
            }
        }
    });

    Ok(UdpListenerHandle { port, _shutdown: tx })
}
