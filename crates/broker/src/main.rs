use anyhow::Result;
use clap::Parser;
use logmon_broker_core::daemon::persistence::{config_dir, load_config};
use logmon_broker_core::daemon::process::is_process_alive;

#[derive(Parser, Debug)]
#[command(name = "logmon-broker", version, about = "logmon broker daemon")]
struct Cli {
    #[command(subcommand)]
    command: Option<Subcommand>,

    /// GELF UDP+TCP port (override config)
    #[arg(long)]
    gelf_port: Option<u16>,
    /// GELF UDP-only override
    #[arg(long)]
    gelf_udp_port: Option<u16>,
    /// GELF TCP-only override
    #[arg(long)]
    gelf_tcp_port: Option<u16>,
    /// OTLP gRPC port (0 = disabled)
    #[arg(long)]
    otlp_grpc_port: Option<u16>,
    /// OTLP HTTP port (0 = disabled)
    #[arg(long)]
    otlp_http_port: Option<u16>,
    /// Log ring buffer capacity
    #[arg(long)]
    buffer_size: Option<usize>,
    /// Span store capacity
    #[arg(long)]
    span_buffer_size: Option<usize>,
}

#[derive(Parser, Debug)]
enum Subcommand {
    /// Print broker status (running pid + socket, or "not running")
    Status,
    // install-service / uninstall-service added in Task 23
}

/// Print broker status. Returns the desired process exit code:
/// `0` when a live broker is found, `1` otherwise (no pid file, stale pid file,
/// or unparseable pid).
fn status() -> Result<i32> {
    let dir = config_dir();
    let pid_path = dir.join("daemon.pid");
    let socket_path = dir.join("logmon.sock");

    if !pid_path.exists() {
        println!("not running");
        return Ok(1);
    }
    let pid_str = std::fs::read_to_string(&pid_path)?;
    let pid: u32 = match pid_str.trim().parse() {
        Ok(p) => p,
        Err(_) => {
            println!("not running (unparseable pid file)");
            return Ok(1);
        }
    };
    if !is_process_alive(pid) {
        println!("not running (stale pid file)");
        return Ok(1);
    }
    println!("running pid={pid} socket={}", socket_path.display());
    Ok(0)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Subcommand::Status) => {
            std::process::exit(status()?);
        }
        None => {
            let mut config = load_config(&config_dir().join("config.json"))?;
            if let Some(p) = cli.gelf_port {
                config.gelf_port = p;
            }
            if let Some(p) = cli.gelf_udp_port {
                config.gelf_udp_port = Some(p);
            }
            if let Some(p) = cli.gelf_tcp_port {
                config.gelf_tcp_port = Some(p);
            }
            if let Some(p) = cli.otlp_grpc_port {
                config.otlp_grpc_port = p;
            }
            if let Some(p) = cli.otlp_http_port {
                config.otlp_http_port = p;
            }
            if let Some(s) = cli.buffer_size {
                config.buffer_size = s;
            }
            if let Some(s) = cli.span_buffer_size {
                config.span_buffer_size = s;
            }
            logmon_broker_core::daemon::server::run_daemon(config).await
        }
    }
}
