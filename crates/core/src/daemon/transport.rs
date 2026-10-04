//! Server-side JSON-RPC transport helpers.
//!
//! Small newline-delimited-JSON read/write helpers used by the daemon accept
//! loop. Duplicated (rather than shared) with the legacy crate's
//! `rpc::transport` since the helpers are tiny and the two crates have
//! divergent shutdown timelines: legacy keeps its copy until Task 5, after
//! which only this copy will remain.
use logmon_broker_protocol::RpcRequest;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

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
#[derive(Default)]
pub struct RequestReader {
    pending: Vec<u8>,
}

impl RequestReader {
    /// The next request, or `None` on EOF.
    pub async fn next<R: AsyncBufReadExt + Unpin>(
        &mut self,
        reader: &mut R,
    ) -> anyhow::Result<Option<RpcRequest>> {
        let n = reader.read_until(b'\n', &mut self.pending).await?;
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
