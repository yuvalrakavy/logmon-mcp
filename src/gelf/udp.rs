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
                    if let Ok((len, _addr)) = result {
                        let seq = pipeline.assign_seq();
                        match parse_gelf_message(&buf[..len], seq) {
                            Ok(entry) => { pipeline.process(entry); }
                            Err(e) => {
                                eprintln!("malformed GELF UDP: {e}");
                                pipeline.increment_malformed();
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
