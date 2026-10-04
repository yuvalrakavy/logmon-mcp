use crate::gelf::message::{parse_gelf_message, LogEntry};
use crate::receiver::keepalive::keep_alive;
use crate::receiver::{ReceiverMetrics, ReceiverSource};
use crate::throttle::{note, pace_after_error, Throttle};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

pub struct TcpListenerHandle {
    port: u16,
    connected_clients: Arc<AtomicU32>,
    /// Dropping it ends the accept loop AND every connection the loop accepted. Ending only
    /// the accept loop left each connection reading into a channel nothing drained once its
    /// domain was deleted — and a sender on a persistent connection then fed a re-created
    /// domain on the same port nothing until it happened to reconnect.
    _shutdown: watch::Sender<()>,
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
/// `gelf_tcp_oversize_dropped`), and the reader skips to the NUL that ends it in bounded memory
/// and carries on: JSON cannot contain a raw NUL, so the next message starts cleanly.
///
/// This bounds what one connection buffers — its read buffer, which can grow to twice the
/// limit — not what the store holds; see the README's "Memory" note.
pub const MAX_GELF_TCP_MESSAGE_BYTES: usize = 64 * 1024;

pub async fn start_tcp_listener(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
    metrics: Arc<ReceiverMetrics>,
) -> anyhow::Result<TcpListenerHandle> {
    start_tcp_listener_with_limit(addr, sender, metrics, MAX_GELF_TCP_MESSAGE_BYTES).await
}

// Throttled, because a remote sender can repeat each of these at will.
static MALFORMED: Throttle = Throttle::new();
static READ_ERRORS: Throttle = Throttle::new();
static ACCEPT_ERRORS: Throttle = Throttle::new();

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
    let (tx, mut stop) = watch::channel(());
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
                            pace_after_error(&ACCEPT_ERRORS, &mut failures_in_a_row, |n| {
                                note(format_args!("GELF TCP accept failed ({n} so far): {e}"));
                            })
                            .await;
                            continue;
                        }
                    };
                    keep_alive(&stream);
                    let (sender, metrics, mut stop) = (sender.clone(), metrics.clone(), stop.clone());
                    connected_clone.fetch_add(1, Ordering::Relaxed);
                    let open = Open(connected_clone.clone());
                    tokio::spawn(async move {
                        let _open = open;
                        // `changed` resolves when the handle's sender is dropped.
                        tokio::select! {
                            () = serve_connection(stream, &sender, &metrics, max_message) => {}
                            _ = stop.changed() => {}
                        }
                    });
                }
                _ = stop.changed() => break,
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
                if let Some(n) = READ_ERRORS.hit() {
                    note(format_args!("GELF TCP read error ({n} so far): {e}"));
                }
                return;
            }
        };
        if bytes_read == 0 {
            return; // EOF
        }
        if buf.len() > max_message {
            metrics.record_oversize_drop();
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
                if let Some(n) = MALFORMED.hit() {
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

    const PAYLOAD: &[u8] =
        br#"{"version":"1.1","host":"h","short_message":"after","level":6,"timestamp":1.0}"#;

    /// A listener whose limit is exactly [`PAYLOAD`] and its NUL.
    async fn small_listener() -> (
        TcpListenerHandle,
        mpsc::Receiver<LogEntry>,
        Arc<ReceiverMetrics>,
    ) {
        let (sender, rx) = mpsc::channel(16);
        let metrics = Arc::new(ReceiverMetrics::new());
        let handle = start_tcp_listener_with_limit(
            "127.0.0.1:0",
            sender,
            metrics.clone(),
            PAYLOAD.len() + 1,
        )
        .await
        .unwrap();
        (handle, rx, metrics)
    }

    async fn until_no_connection(handle: &TcpListenerHandle) {
        for _ in 0..500 {
            if handle.connected_clients() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            handle.connected_clients(),
            0,
            "the connection is still counted"
        );
    }

    /// A message past the limit is dropped — counted as oversize, NOT as a receiver drop, whose
    /// remedy is a different one — without buffering it whole, and the reader skips to the NUL
    /// that ends it: the next message on the SAME connection arrives. The connection is counted
    /// while open and no longer once it ends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_message_past_the_limit_is_dropped_and_the_next_one_arrives() {
        let (handle, mut rx, metrics) = small_listener().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", handle.port()))
            .await
            .unwrap();
        // An oversize message — longer than the limit by far, sent in pieces — then its NUL,
        // then an ordinary message.
        for _ in 0..50 {
            stream.write_all(&[b'x'; 1000]).await.unwrap();
        }
        stream.write_all(&[0u8]).await.unwrap();
        stream.write_all(PAYLOAD).await.unwrap();
        stream.write_all(&[0u8]).await.unwrap();
        let entry = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the next message arrives")
            .expect("the channel is open");
        assert_eq!(entry.message, "after");
        assert_eq!(
            metrics.oversize_dropped(),
            1,
            "the oversize message is counted"
        );
        assert_eq!(
            metrics.snapshot().gelf_tcp,
            0,
            "an oversize message is not a receiver drop"
        );
        assert_eq!(
            handle.connected_clients(),
            1,
            "the open connection is counted"
        );

        drop(stream);
        until_no_connection(&handle).await;
    }

    /// An oversize message the input ends inside — no NUL ever comes — ends the connection
    /// rather than waiting on it forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_oversize_message_cut_off_by_the_end_of_input_ends_the_connection() {
        let (handle, _rx, metrics) = small_listener().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", handle.port()))
            .await
            .unwrap();
        stream.write_all(&[b'x'; 30_000]).await.unwrap();
        stream.shutdown().await.unwrap();
        // Counted first, so the connection is known to have been accepted and served: a zero
        // count of open connections means nothing before that.
        for _ in 0..500 {
            if metrics.oversize_dropped() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(metrics.oversize_dropped(), 1);
        until_no_connection(&handle).await;
        drop(stream);
    }

    /// Dropping the listener — what deleting its domain does — closes the connections it
    /// accepted, not just the accept loop: the sender sees its connection end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_listener_closes_its_connections() {
        let (handle, mut rx, _metrics) = small_listener().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", handle.port()))
            .await
            .unwrap();
        stream.write_all(PAYLOAD).await.unwrap();
        stream.write_all(&[0u8]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the connection is being served")
            .expect("the channel is open");

        drop(handle);
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut byte))
            .await
            .expect("the connection was closed when its listener went");
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
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
