use clap::Parser;
use logmon_broker_sdk::Broker;
use rmcp::ServiceExt;

mod auto_start;
mod notifications;
mod server;

#[derive(Parser, Debug)]
#[command(name = "logmon-mcp", version, about = "logmon MCP shim")]
struct Cli {
    /// Optional named session
    #[arg(long)]
    session: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("logmon_mcp=info".parse()?),
        )
        .init();

    let cli = Cli::parse();

    // Auto-start broker if needed (Task 21 fills in the discovery logic)
    auto_start::ensure_broker_running().await?;

    let mut builder = Broker::connect();
    if let Some(name) = cli.session {
        builder = builder.session_name(name);
    }
    let broker = builder.open().await?;

    // Start MCP server, forwarding notifications
    let mcp_server = server::GelfMcpServer::new(broker.clone());
    let service = mcp_server.serve(rmcp::transport::stdio()).await?;

    notifications::spawn_notification_forwarder(
        broker.subscribe_notifications(),
        service.peer().clone(),
    );

    service.waiting().await?;
    Ok(())
}
