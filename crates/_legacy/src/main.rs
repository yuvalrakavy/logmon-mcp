use clap::Parser;
use logmon_mcp_server::config::{Cli, Commands};
use logmon_mcp_server::daemon::persistence::{config_dir, load_config};
use logmon_mcp_server::daemon::server::run_daemon;
use logmon_mcp_server::shim::auto_start::connect_to_daemon;
use logmon_mcp_server::shim::bridge::run_shim;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Daemon {
            gelf_port,
            gelf_udp_port,
            gelf_tcp_port,
            buffer_size,
            otlp_grpc_port,
            otlp_http_port,
            span_buffer_size,
        }) => {
            // Daemon mode: load config file, apply CLI overrides
            let mut config = load_config(&config_dir().join("config.json"))?;
            config.gelf_port = gelf_port;
            if let Some(p) = gelf_udp_port {
                config.gelf_udp_port = Some(p);
            }
            if let Some(p) = gelf_tcp_port {
                config.gelf_tcp_port = Some(p);
            }
            config.buffer_size = buffer_size;
            config.otlp_grpc_port = otlp_grpc_port;
            config.otlp_http_port = otlp_http_port;
            config.span_buffer_size = span_buffer_size;
            run_daemon(config).await
        }
        None => {
            // Shim mode: connect to daemon (auto-starting if needed), run MCP on stdio
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::from_default_env()
                        .add_directive("logmon_mcp_server=info".parse()?),
                )
                .init();

            let conn = connect_to_daemon().await?;
            run_shim(conn, cli.session).await
        }
    }
}
