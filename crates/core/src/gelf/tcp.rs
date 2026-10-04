use crate::gelf::message::{parse_gelf_message, LogEntry};
use crate::receiver::{ReceiverMetrics, ReceiverSource};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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
/// same ceiling a UDP datagram has, so the TCP input accepts no larger message than UDP does.
/// A message was read to its NUL with no limit, on a port bound to every interface without
/// authentication — so any host that could reach it could stream bytes with no NUL and grow the
/// broker's memory until it died. A longer message is dropped (counted in
/// `receiver_drops.gelf_tcp`), and the reader skips to the NUL that ends it in bounded memory
/// and carries on: JSON cannot contain a raw NUL, so the next message starts cleanly.
///
/// This bounds what one connection buffers, not what the store holds — see the README's
/// "Memory" note.
pub const MAX_GELF_TCP_MESSAGE_BYTES: usize = 64 * 1024;

pub async fn start_tcp_listener(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
    metrics: Arc<ReceiverMetrics>,
) -> anyhow::Result<TcpListenerHandle> {
    start_tcp_listener_with_limit(addr, sender, metrics, MAX_GELF_TCP_MESSAGE_BYTES).await
}

/// Write a line to stderr without ever panicking. `eprintln!` panics when stderr cannot be
/// written — a full disk under the auto-started broker's log file — and the panic killed the
/// task that logged: a connection, or the accept loop itself.
pub(crate) fn note(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{args}");
}

/// Whether to log the `count`th occurrence of something a remote sender can repeat at will:
/// the 1st, 2nd, 4th, 8th… — so an attacker cannot fill the disk through the log.
pub(crate) fn log_now(count: u64) -> bool {
    count.is_power_of_two()
}

static MALFORMED: AtomicU64 = AtomicU64::new(0);
static READ_ERRORS: AtomicU64 = AtomicU64::new(0);
static ACCEPT_ERRORS: AtomicU64 = AtomicU64::new(0);

/// Counts an open connection, and uncounts it when its task ends — however it ends.
struct Open(Arc<AtomicU32>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
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
        let mut failures_in_a_row = 0u32;
        loop {
            tokio::select! {
                result = listener.accept() => {
                    let stream = match result {
                        Ok((stream, _addr)) => {
                            failures_in_a_row = 0;
                            stream
                        }
                        Err(e) => {
                            // A one-off failure (a peer that reset before it was accepted) is
                            // retried at once. One that repeats — out of file descriptors,
                            // typically — was retried at once too, returning the same error at
                            // once: a spin at full CPU for as long as it lasted.
                            failures_in_a_row = failures_in_a_row.saturating_add(1);
                            let n = ACCEPT_ERRORS.fetch_add(1, Ordering::Relaxed) + 1;
                            if log_now(n) {
                                note(format_args!("GELF TCP accept failed ({n} so far): {e}"));
                            }
                            if failures_in_a_row > 1 {
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            }
                            continue;
                        }
                    };
                    let (sender, metrics) = (sender.clone(), metrics.clone());
                    connected_clone.fetch_add(1, Ordering::Relaxed);
                    let open = Open(connected_clone.clone());
                    tokio::spawn(async move {
                        let _open = open;
                        serve_connection(stream, &sender, &metrics, max_message).await;
                    });
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

/// Read NUL-terminated GELF messages from one connection until it ends.
async fn serve_connection(
    stream: tokio::net::TcpStream,
    sender: &mpsc::Sender<LogEntry>,
    metrics: &ReceiverMetrics,
    max_message: usize,
) {
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
                let n = READ_ERRORS.fetch_add(1, Ordering::Relaxed) + 1;
                if log_now(n) {
                    note(format_args!("GELF TCP read error ({n} so far): {e}"));
                }
                return;
            }
        };
        if bytes_read == 0 {
            return; // EOF
        }
        if buf.len() > max_message {
            metrics.record_oversize_drop(ReceiverSource::GelfTcp);
            if buf.last() != Some(&0) && !skip_past_nul(&mut reader, &mut buf).await {
                return;
            }
            continue;
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
                let _ = metrics.try_send_log(sender, entry, ReceiverSource::GelfTcp);
            }
            Err(e) => {
                let n = MALFORMED.fetch_add(1, Ordering::Relaxed) + 1;
                if log_now(n) {
                    note(format_args!("malformed GELF TCP ({n} so far): {e}"));
                }
            }
        }
    }
}

/// Discard input through the next NUL, a bounded chunk at a time. `false` at end of input or
/// on a read error.
async fn skip_past_nul(reader: &mut BufReader<tokio::net::TcpStream>, buf: &mut Vec<u8>) -> bool {
    const CHUNK: u64 = 8 * 1024;
    loop {
        buf.clear();
        match (&mut *reader).take(CHUNK).read_until(b'\0', buf).await {
            Ok(0) | Err(_) => return false,
            Ok(_) if buf.last() == Some(&0) => return true,
            Ok(_) => {}
        }
    }
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

    /// A message past the limit is dropped — counted in `receiver_drops.gelf_tcp` — without
    /// buffering it whole, and the reader skips to the NUL that ends it: the next message on the
    /// SAME connection arrives. When the connection ends, it is no longer counted as open.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_message_past_the_limit_is_dropped_and_the_next_one_arrives() {
        let (sender, mut rx) = mpsc::channel(16);
        let metrics = Arc::new(ReceiverMetrics::new());
        let payload =
            br#"{"version":"1.1","host":"h","short_message":"after","level":6,"timestamp":1.0}"#;
        let handle = start_tcp_listener_with_limit(
            "127.0.0.1:0",
            sender,
            metrics.clone(),
            payload.len() + 1,
        )
        .await
        .unwrap();
        let addr = format!("127.0.0.1:{}", handle.port());

        let mut stream = TcpStream::connect(&addr).await.unwrap();
        // An oversize message — longer than the limit by far, sent in pieces — then its NUL,
        // then an ordinary message.
        for _ in 0..50 {
            stream.write_all(&[b'x'; 1000]).await.unwrap();
        }
        stream.write_all(&[0u8]).await.unwrap();
        stream.write_all(payload).await.unwrap();
        stream.write_all(&[0u8]).await.unwrap();
        let entry = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the next message arrives")
            .expect("the channel is open");
        assert_eq!(entry.message, "after");
        assert_eq!(
            metrics.snapshot().gelf_tcp,
            1,
            "the oversize message is counted"
        );

        drop(stream);
        for _ in 0..500 {
            if handle.connected_clients() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(handle.connected_clients(), 0);
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
