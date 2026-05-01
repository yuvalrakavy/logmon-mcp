use logmon_broker_protocol::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// Write a JSON-RPC message followed by newline
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

/// Read one newline-delimited JSON line. Returns None on EOF.
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

/// Read and parse a DaemonMessage (response or notification)
pub async fn read_daemon_message<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<Option<DaemonMessage>> {
    match read_line(reader).await? {
        Some(line) => Ok(Some(parse_daemon_message_from_str(&line)?)),
        None => Ok(None),
    }
}

/// Read and parse an RpcRequest
pub async fn read_request<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<Option<RpcRequest>> {
    match read_line(reader).await? {
        Some(line) => Ok(Some(serde_json::from_str(&line)?)),
        None => Ok(None),
    }
}
