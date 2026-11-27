use futures_util::{SinkExt, StreamExt, future, stream};
use rmcp::{
    RoleServer,
    model::{ClientJsonRpcMessage, ErrorCode, ErrorData, ServerJsonRpcMessage},
    transport::{IntoTransport, Transport, async_rw::JsonRpcMessageCodec},
};
use tokio_util::codec::{FramedRead, FramedWrite};

pub async fn transport() -> std::io::Result<impl Transport<RoleServer, Error = std::io::Error>> {
    let mut input = FramedRead::new(
        tokio::io::stdin(),
        JsonRpcMessageCodec::<ClientJsonRpcMessage>::default(),
    );
    let mut output = FramedWrite::new(
        tokio::io::stdout(),
        JsonRpcMessageCodec::<ServerJsonRpcMessage>::default(),
    )
    .sink_map_err(std::io::Error::from);

    // Antigravity uses MethodNotFound to fall back to initialize on the same pipe.
    let first = match input
        .next()
        .await
        .transpose()
        .map_err(std::io::Error::from)?
    {
        Some(ClientJsonRpcMessage::Request(request))
            if request.request.method() == "server/discover" =>
        {
            tracing::debug!("received MCP discovery probe before initialize");
            output
                .send(ServerJsonRpcMessage::error(
                    ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "Use MCP initialize", None),
                    request.id,
                ))
                .await?;
            None
        }
        frame => frame,
    };
    let input = input
        .take_while(|message| {
            if let Err(error) = message {
                tracing::error!("Error reading from stream: {error}");
            }
            future::ready(message.is_ok())
        })
        .filter_map(|message| future::ready(message.ok()));
    Ok((output, stream::iter(first).chain(input)).into_transport())
}
