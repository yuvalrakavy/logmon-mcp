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

/// The longest GELF message one TCP connection may send, its NUL terminator included — the
/// same ceiling a UDP datagram has, so the TCP input lets a sender put no larger record into
/// the store than UDP already does. A message was read to its NUL with no limit, on a port
/// bound to every interface without authentication — so any host that could reach it could
/// stream bytes with no NUL and grow the broker's memory until it died. A longer message closes
/// its connection: what follows it cannot be told apart from the next message.
///
/// (The store itself is bounded by record COUNT, not bytes, for every input — UDP included:
/// the most it can hold is its buffer size times this. See the README's known limits.)
pub const MAX_GELF_TCP_MESSAGE_BYTES: usize = 64 * 1024;

/// How many GELF TCP connections may be open at once. Each holds a read buffer of up to two
/// [`MAX_GELF_TCP_MESSAGE_BYTES`] (with the allocator's growth), so this bounds what the TCP
/// input can hold in total; a connection past it is closed on accept.
pub const MAX_GELF_TCP_CONNECTIONS: u32 = 128;

pub async fn start_tcp_listener(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
    metrics: Arc<ReceiverMetrics>,
) -> anyhow::Result<TcpListenerHandle> {
    start_tcp_listener_with_limits(
        addr,
        sender,
        metrics,
        MAX_GELF_TCP_MESSAGE_BYTES,
        MAX_GELF_TCP_CONNECTIONS,
    )
    .await
}

/// Log the 1st, 2nd, 4th, 8th... occurrence of something an attacker can repeat at will.
fn log_now(count: u64) -> bool {
    count.is_power_of_two()
}

async fn start_tcp_listener_with_limits(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
    metrics: Arc<ReceiverMetrics>,
    max_message: usize,
    max_connections: u32,
) -> anyhow::Result<TcpListenerHandle> {
    let listener = TcpListener::bind(addr).await?;
    let port = listener.local_addr()?.port();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let connected = Arc::new(AtomicU32::new(0));
    let connected_clone = connected.clone();

    tokio::spawn(async move {
        let (mut refused, mut accept_errors) = (0u64, 0u64);
        loop {
            tokio::select! {
                result = listener.accept() => {
                    let stream = match result {
                        Ok((stream, _addr)) => stream,
                        Err(e) => {
                            // Out of file descriptors, typically. Retrying at once returned
                            // the same error at once — a spin at full CPU for as long as it
                            // lasted.
                            accept_errors += 1;
                            if log_now(accept_errors) {
                                eprintln!("GELF TCP accept failed ({accept_errors} so far): {e}");
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    if connected_clone.load(Ordering::Relaxed) >= max_connections {
                        refused += 1;
                        if log_now(refused) {
                            eprintln!(
                                "GELF TCP: {max_connections} connections open; closed a new one \
                                 ({refused} so far)"
                            );
                        }
                        drop(stream);
                        continue;
                    }
                    {
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
        let handle = start_tcp_listener_with_limits(
            "127.0.0.1:0",
            sender,
            metrics,
            payload.len() + 1,
            MAX_GELF_TCP_CONNECTIONS,
        )
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

    /// Past the connection cap a new connection is closed on accept, so the TCP input's total
    /// buffers stay bounded however many connections a sender opens; once one closes, a new
    /// one is served again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_past_the_cap_is_closed() {
        use tokio::io::AsyncReadExt;
        let (sender, mut rx) = mpsc::channel(16);
        let metrics = Arc::new(ReceiverMetrics::new());
        let handle = start_tcp_listener_with_limits(
            "127.0.0.1:0",
            sender,
            metrics,
            MAX_GELF_TCP_MESSAGE_BYTES,
            2,
        )
        .await
        .unwrap();
        let addr = format!("127.0.0.1:{}", handle.port());

        let held = [
            TcpStream::connect(&addr).await.unwrap(),
            TcpStream::connect(&addr).await.unwrap(),
        ];
        for _ in 0..500 {
            if handle.connected_clients() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            handle.connected_clients(),
            2,
            "both held connections are served"
        );

        let mut third = TcpStream::connect(&addr).await.unwrap();
        let mut byte = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(10), third.read(&mut byte))
            .await
            .expect("the third is closed")
            .unwrap_or(0);
        assert_eq!(n, 0, "closed, not served");

        drop(held);
        for _ in 0..500 {
            if handle.connected_clients() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let payload =
            br#"{"version":"1.1","host":"h","short_message":"after","level":6,"timestamp":1.0}"#;
        let mut again = TcpStream::connect(&addr).await.unwrap();
        again.write_all(payload).await.unwrap();
        again.write_all(&[0u8]).await.unwrap();
        let entry = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("served again")
            .expect("the channel is open");
        assert_eq!(entry.message, "after");
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
