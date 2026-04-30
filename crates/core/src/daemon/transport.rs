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

/// Read and parse one `RpcRequest`. Returns `None` on EOF.
pub async fn read_request<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<Option<RpcRequest>> {
    match read_line(reader).await? {
        Some(line) => Ok(Some(serde_json::from_str(&line)?)),
        None => Ok(None),
    }
}
