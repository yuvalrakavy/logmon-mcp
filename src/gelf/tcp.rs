use crate::engine::pipeline::LogPipeline;
use crate::gelf::message::parse_gelf_message;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;

pub struct TcpListenerHandle {
    port: u16,
    connected_clients: Arc<AtomicU32>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl TcpListenerHandle {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn connected_clients(&self) -> u32 {
        self.connected_clients.load(Ordering::Relaxed)
    }
}

pub async fn start_tcp_listener(
    addr: &str,
    pipeline: Arc<LogPipeline>,
) -> anyhow::Result<TcpListenerHandle> {
    let listener = TcpListener::bind(addr).await?;
    let port = listener.local_addr()?.port();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let connected = Arc::new(AtomicU32::new(0));
    let connected_clone = connected.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = listener.accept() => {
                    if let Ok((stream, _addr)) = result {
                        let pipeline = pipeline.clone();
                        let connected = connected_clone.clone();
                        connected.fetch_add(1, Ordering::Relaxed);

                        tokio::spawn(async move {
                            let mut reader = BufReader::new(stream);
                            let mut buf = Vec::new();

                            loop {
                                buf.clear();
                                let bytes_read = match reader.read_until(b'\0', &mut buf).await {
                                    Ok(n) => n,
                                    Err(e) => {
                                        eprintln!("TCP read error: {e}");
                                        break;
                                    }
                                };

                                if bytes_read == 0 {
                                    break; // EOF
                                }

                                // Remove trailing null byte
                                if buf.last() == Some(&0) {
                                    buf.pop();
                                }

                                if buf.is_empty() {
                                    continue;
                                }

                                let seq = pipeline.assign_seq();
                                match parse_gelf_message(&buf, seq) {
                                    Ok(entry) => {
                                        pipeline.process(entry);
                                    }
                                    Err(e) => {
                                        eprintln!("malformed GELF TCP: {e}");
                                        pipeline.increment_malformed();
                                    }
                                }
                            }

                            connected.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                }
                _ = &mut rx => break,
            }
        }
    });

    Ok(TcpListenerHandle {
        port,
        connected_clients: connected,
        _shutdown: tx,
    })
}
