use clap::{Parser, Subcommand as ClapSubcommand};
use logmon_broker_sdk::Broker;
use rmcp::ServiceExt;

mod auto_start;
mod cli;
mod notifications;
mod server;

#[derive(Parser, Debug)]
#[command(
    name = "logmon-mcp",
    version,
    about = "logmon broker MCP shim and CLI tool"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Subcommand>,

    /// Named session to attach to. Default for MCP stdio mode is anonymous.
    /// Default for CLI mode is "cli".
    #[arg(long, global = true)]
    session: Option<String>,

    /// Emit machine-readable JSON instead of human-readable text. CLI mode only.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(ClapSubcommand, Debug)]
enum Subcommand {
    /// Query and clear log entries.
    Logs(cli::logs::LogsCmd),
    /// Manage bookmarks (named seq positions, also referenced as cursors via c>=).
    Bookmarks(cli::bookmarks::BookmarksCmd),
    /// Manage triggers (filter-driven notifications fired on log match).
    Triggers(cli::triggers::TriggersCmd),
    /// Manage buffer filters (per-session).
    Filters(cli::filters::FiltersCmd),
    /// Query traces and trace summaries.
    Traces(cli::traces::TracesCmd),
    /// Query span context.
    Spans(cli::spans::SpansCmd),
    /// List or drop sessions.
    Sessions(cli::sessions::SessionsCmd),
    /// Print broker status (uptime, receivers, store stats).
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(cmd) => {
            // CLI mode — short-lived, fail-fast, route to subcommand handler.
            // No tracing init: CLI is silent on stderr unless format::error()
            // explicitly writes there. Stray RUST_LOG settings shouldn't leak
            // SDK warnings into a CLI consumer's stderr stream.
            let exit_code = cli::dispatch(cmd, cli.session, cli.json).await;
            std::process::exit(exit_code);
        }
        None => {
            // MCP stdio mode — today's path, unchanged.
            run_mcp_stdio(cli.session).await
        }
    }
}

async fn run_mcp_stdio(session: Option<String>) -> anyhow::Result<()> {
    // Tracing init lives here, not in main(), so CLI mode stays silent on
    // stderr (CLI consumers expect format::error() output, not interleaved
    // tracing noise).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("logmon_mcp=info".parse()?),
        )
        .init();

    auto_start::ensure_broker_running().await?;

    let mut builder = Broker::connect();
    if let Some(name) = session {
        builder = builder.session_name(name);
    }
    let broker = builder.open().await?;

    let mcp_server = server::GelfMcpServer::new(broker.clone());
    let service = mcp_server.serve(rmcp::transport::stdio()).await?;

    notifications::spawn_notification_forwarder(
        broker.subscribe_notifications(),
        service.peer().clone(),
    );

    service.waiting().await?;
    Ok(())
}
