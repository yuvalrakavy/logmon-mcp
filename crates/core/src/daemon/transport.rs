//! Server-side JSON-RPC transport helpers.
//!
//! Small newline-delimited-JSON read/write helpers used by the daemon accept
//! loop. Duplicated (rather than shared) with the legacy crate's
//! `rpc::transport` since the helpers are tiny and the two crates have
//! divergent shutdown timelines: legacy keeps its copy until Task 5, after
//! which only this copy will remain.
use logmon_broker_protocol::RpcRequest;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

/// Write a JSON-RPC message followed by a newline.
pub async fn write_message<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &impl serde::Serialize,
) -> anyhow::Result<()> {
    let json = serde_json::to_string(msg)?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

/// Read one newline-delimited JSON line. Returns `None` on EOF.
pub async fn read_line<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<Option<String>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

/// Reads newline-delimited requests and can be raced in `select!`: a request only partly
/// received when another branch wins stays in the reader's buffer, and the next call carries
/// on from where that one stopped.
///
/// [`read_request`] cannot be raced. It reads into a buffer of its own, which the losing
/// branch drops with whatever had arrived; the rest of the line then failed to parse and the
/// connection closed. The connection loop races its reads against trigger notifications, and
/// clients write a request and its newline separately — a trigger firing between them was
/// enough. This relies on `read_until`'s documented guarantee that what a cancelled call read
/// is already in the buffer it was given.
///
/// A request is at most [`MAX_REQUEST_BYTES`]. A line that reaches it without a newline is an
/// error, which closes the connection: unbounded, a client streaming bytes with no newline
/// grew the buffer until the daemon ran out of memory.
#[derive(Default)]
pub struct RequestReader {
    pending: Vec<u8>,
}

/// The longest request line [`RequestReader`] accepts, newline included — far above any real
/// request, which is a filter and some parameters.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

impl RequestReader {
    /// The next request, or `None` on EOF.
    pub async fn next<R: AsyncBufReadExt + Unpin>(
        &mut self,
        reader: &mut R,
    ) -> anyhow::Result<Option<RpcRequest>> {
        self.next_limited(reader, MAX_REQUEST_BYTES).await
    }

    async fn next_limited<R: AsyncBufReadExt + Unpin>(
        &mut self,
        reader: &mut R,
        max: usize,
    ) -> anyhow::Result<Option<RpcRequest>> {
        // One byte past the limit is enough to know it was passed; `take` is rebuilt per call,
        // so a cancelled call's bytes (already in `pending`) count against the next one's.
        let room = (max + 1).saturating_sub(self.pending.len()) as u64;
        let n = (&mut *reader)
            .take(room)
            .read_until(b'\n', &mut self.pending)
            .await?;
        if self.pending.len() > max {
            self.pending.clear();
            anyhow::bail!("request line exceeds {max} bytes");
        }
        if n == 0 && self.pending.is_empty() {
            return Ok(None);
        }
        let line = std::mem::take(&mut self.pending);
        Ok(Some(serde_json::from_slice(&line)?))
    }
}

/// Read and parse one `RpcRequest`. Returns `None` on EOF. Not for a `select!` branch — see
/// [`RequestReader`].
pub async fn read_request<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<Option<RpcRequest>> {
    match read_line(reader).await? {
        Some(line) => Ok(Some(serde_json::from_str(&line)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::io::BufReader;

    fn request_bytes(id: u64) -> Vec<u8> {
        serde_json::to_vec(&RpcRequest::new(id, "status.get", json!({}))).unwrap()
    }

    /// A read cancelled with half a request buffered keeps that half, and the next read
    /// completes the request — rather than failing to parse its tail, which closed the
    /// connection.
    #[tokio::test]
    async fn a_cancelled_read_keeps_the_partial_request() {
        let (mut client, server) = tokio::io::duplex(4096);
        let mut reader = BufReader::new(server);
        let mut requests = RequestReader::default();
        let line = request_bytes(7);
        let half = line.len() / 2;
        client.write_all(&line[..half]).await.unwrap();
        // The losing branch of a `select!`: polled once — consuming the half already sent —
        // then dropped.
        let raced =
            tokio::time::timeout(Duration::from_millis(50), requests.next(&mut reader)).await;
        assert!(raced.is_err(), "nothing complete to return yet");
        client.write_all(&line[half..]).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        let got = requests
            .next(&mut reader)
            .await
            .unwrap()
            .expect("a request");
        assert_eq!(got.id, 7);
    }

    /// A line past the limit is refused instead of buffered without end; one at the limit is
    /// read.
    #[tokio::test]
    async fn a_request_line_past_the_limit_is_refused() {
        let line = request_bytes(9);
        let (mut client, server) = tokio::io::duplex(4096);
        let mut reader = BufReader::new(server);
        let mut requests = RequestReader::default();
        client.write_all(&line).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        let at_limit = requests
            .next_limited(&mut reader, line.len() + 1)
            .await
            .unwrap()
            .expect("a request at the limit");
        assert_eq!(at_limit.id, 9);
        client.write_all(&line).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        let err = requests
            .next_limited(&mut reader, line.len())
            .await
            .expect_err("a request past the limit");
        assert!(err.to_string().contains("exceeds"), "{err}");
    }
}
