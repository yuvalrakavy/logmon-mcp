use logmon_broker_sdk::Broker;
use rmcp::ServiceExt;

mod auto_start;
mod cli;
mod notifications;
mod server;

/// Argument parsing is **not** done with clap.
///
/// clap builds its command tree before argv is parsed, and the tree is not
/// known until the daemon has answered `tools.manifest`. A clap-shaped CLI
/// would have to declare every command up front — which is precisely the
/// hardcoded knowledge of the daemon that this shim is meant not to have.
/// [`cli::split_globals`] separates this process's own flags; everything after
/// them is the command, and the daemon describes it.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut globals, argv) = cli::split_globals(&args);

    if globals.version {
        println!("logmon-mcp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // `--domain` wins; else the `LOGMON_DOMAIN` env var, so a per-worktree MCP
    // config (or shell) can set the track's domain once and every session/CLI
    // call auto-binds it. An empty env var is treated as unset.
    globals.domain = globals.domain.or_else(|| {
        std::env::var("LOGMON_DOMAIN")
            .ok()
            .filter(|s| !s.trim().is_empty())
    });

    // A command was named, or help was asked for -> CLI mode. Bare invocation
    // with no arguments is MCP stdio mode, which is how an MCP client starts it.
    if !argv.is_empty() || globals.help {
        // No tracing init: CLI is silent on stderr unless format::error()
        // explicitly writes there. Stray RUST_LOG settings shouldn't leak
        // SDK warnings into a CLI consumer's stderr stream.
        let exit_code = cli::dispatch(globals, argv).await;
        std::process::exit(exit_code);
    }

    run_mcp_stdio(globals.session, globals.domain).await
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

    let mcp_server = server::GelfMcpServer::taught_by(broker.clone()).await?;
    let service = mcp_server.serve(rmcp::transport::stdio()).await?;

    notifications::spawn_notification_forwarder(
        broker.subscribe_notifications(),
        service.peer().clone(),
    );

    service.waiting().await?;
    Ok(())
}
