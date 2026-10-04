use crate::gelf::message::{parse_gelf_message, LogEntry};
use crate::receiver::{ReceiverMetrics, ReceiverSource};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

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

/// The longest GELF message one TCP connection may send, its NUL terminator included. A
/// message was read to its NUL with no limit, on a port bound to every interface without
/// authentication — so any host that could reach it could stream bytes with no NUL and grow the
/// daemon's memory until it died. Generous next to real logs (chunked GELF over UDP tops out
/// near 1 MB; Graylog's own TCP input defaults to 2 MB). A longer message closes its
/// connection: what follows it cannot be told apart from the next message.
pub const MAX_GELF_TCP_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

pub async fn start_tcp_listener(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
    metrics: Arc<ReceiverMetrics>,
) -> anyhow::Result<TcpListenerHandle> {
    start_tcp_listener_with_limit(addr, sender, metrics, MAX_GELF_TCP_MESSAGE_BYTES).await
}

async fn start_tcp_listener_with_limit(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
    metrics: Arc<ReceiverMetrics>,
    max_message: usize,
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
                        let sender = sender.clone();
                        let metrics = metrics.clone();
                        let connected = connected_clone.clone();
                        connected.fetch_add(1, Ordering::Relaxed);

                        tokio::spawn(async move {
                            let mut reader = BufReader::new(stream);
                            let mut buf = Vec::new();

                            loop {
                                buf.clear();
                                // One byte past the limit is enough to know it was passed.
                                let bytes_read = match (&mut reader)
                                    .take(max_message as u64 + 1)
                                    .read_until(b'\0', &mut buf)
                                    .await
                                {
                                    Ok(n) => n,
                                    Err(e) => {
                                        eprintln!("TCP read error: {e}");
                                        break;
                                    }
                                };

                                if bytes_read == 0 {
                                    break; // EOF
                                }
                                if buf.len() > max_message {
                                    eprintln!(
                                        "GELF TCP message exceeds {max_message} bytes; closing \
                                         the connection"
                                    );
                                    break;
                                }

                                // Remove trailing null byte
                                if buf.last() == Some(&0) {
                                    buf.pop();
                                }

                                if buf.is_empty() {
                                    continue;
                                }

                                // Parse with seq=0 — daemon assigns real seq later
                                match parse_gelf_message(&buf, 0) {
                                    Ok(entry) => {
                                        let _ = metrics.try_send_log(
                                            &sender,
                                            entry,
                                            ReceiverSource::GelfTcp,
                                        );
                                    }
                                    Err(e) => {
                                        eprintln!("malformed GELF TCP: {e}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::ReceiverMetrics;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;

    /// A message past the limit closes its connection instead of growing a buffer without end,
    /// and the listener keeps serving other connections.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_message_past_the_limit_closes_its_connection() {
        use tokio::io::AsyncReadExt;
        let (sender, mut rx) = mpsc::channel(16);
        let metrics = Arc::new(ReceiverMetrics::new());
        let payload =
            br#"{"version":"1.1","host":"h","short_message":"x","level":6,"timestamp":1.0}"#;
        let handle =
            start_tcp_listener_with_limit("127.0.0.1:0", sender, metrics, payload.len() + 1)
                .await
                .unwrap();
        let addr = format!("127.0.0.1:{}", handle.port());

        let mut flood = TcpStream::connect(&addr).await.unwrap();
        flood.write_all(&[b'x'; 200]).await.unwrap();
        let mut byte = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(10), flood.read(&mut byte))
            .await
            .expect("the daemon closes the connection")
            .unwrap_or(0);
        assert_eq!(n, 0, "closed, not answered");

        // A message at the limit still arrives, on a new connection.
        let mut ok = TcpStream::connect(&addr).await.unwrap();
        ok.write_all(payload).await.unwrap();
        ok.write_all(&[0u8]).await.unwrap();
        let entry = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the message arrives")
            .expect("the channel is open");
        assert_eq!(entry.message, "x");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_channel_does_not_park_tcp_listener() {
        let (sender, _rx) = mpsc::channel(1);
        sender
            .try_send(crate::gelf::message::LogEntry {
                seq: 0,
                timestamp: chrono::Utc::now(),
                level: crate::gelf::message::Level::Info,
                message: "filler".into(),
                full_message: None,
                host: "h".into(),
                facility: None,
                file: None,
                line: None,
                additional_fields: std::collections::HashMap::new(),
                trace_id: None,
                span_id: None,
                matched_filters: vec![],
                source: crate::gelf::message::LogSource::Filter,
            })
            .unwrap();

        let metrics = Arc::new(ReceiverMetrics::new());
        let handle = start_tcp_listener("127.0.0.1:0", sender, metrics.clone())
            .await
            .unwrap();
        let port = handle.port();

        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let payload =
            br#"{"version":"1.1","host":"h","short_message":"x","level":6,"timestamp":1.0}"#;
        for _ in 0..50 {
            stream.write_all(payload).await.unwrap();
            stream.write_all(&[0u8]).await.unwrap();
        }
        stream.flush().await.unwrap();
        // Drop stream to signal EOF.
        drop(stream);

        tokio::time::sleep(Duration::from_millis(200)).await;
        let snap = metrics.snapshot();
        assert!(
            snap.gelf_tcp >= 1,
            "expected at least one drop, got {:?}",
            snap
        );
    }
}
