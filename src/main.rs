use logmon_mcp_server::shim::auto_start::connect_to_daemon;
use logmon_mcp_server::shim::bridge::run_shim;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize stderr logging (stdout is reserved for MCP JSON-RPC)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("logmon_mcp_server=info".parse()?),
        )
        .init();

    // Connect to daemon (auto-starting if needed)
    let conn = connect_to_daemon().await?;

    // Run the shim bridge (MCP server on stdio, RPC to daemon)
    run_shim(conn, None).await?;

    Ok(())
}
