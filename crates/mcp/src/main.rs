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

    /// Bind to an existing domain. MCP stdio mode: a connect-time bind, re-applied
    /// on every reconnect (durable — never silently reverts to `default`). CLI
    /// mode: scopes this invocation (queries + `domains clear`; create/delete/list
    /// ignore it). Falls back to the `LOGMON_DOMAIN` env var (set once per
    /// worktree). Omitted → `default`. The domain must exist (fail-loud).
    #[arg(long, global = true)]
    domain: Option<String>,

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
    /// Create, delete, list, or clear domains (isolated broker instances).
    Domains(cli::domains::DomainsCmd),
    /// Print broker status (uptime, receivers, store stats).
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // `--domain` wins; else the `LOGMON_DOMAIN` env var, so a per-worktree MCP
    // config (or shell) can set the track's domain once and every session/CLI
    // call auto-binds it. An empty env var is treated as unset.
    let domain = cli.domain.or_else(|| {
        std::env::var("LOGMON_DOMAIN")
            .ok()
            .filter(|s| !s.trim().is_empty())
    });

    match cli.command {
        Some(cmd) => {
            // CLI mode — short-lived, fail-fast, route to subcommand handler.
            // No tracing init: CLI is silent on stderr unless format::error()
            // explicitly writes there. Stray RUST_LOG settings shouldn't leak
            // SDK warnings into a CLI consumer's stderr stream.
            let exit_code = cli::dispatch(cmd, cli.session, domain, cli.json).await;
            std::process::exit(exit_code);
        }
        None => {
            // MCP stdio mode.
            run_mcp_stdio(cli.session, domain).await
        }
    }
}

async fn run_mcp_stdio(session: Option<String>, domain: Option<String>) -> anyhow::Result<()> {
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
    if let Some(d) = domain {
        // Connect-time bind (durable across reconnects). Fail-loud if the domain
        // doesn't exist — the shim refuses to start rather than silently serving
        // `default`. (Create the domain before connecting the shim.)
        builder = builder.domain(d);
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
