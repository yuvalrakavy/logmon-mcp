use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "logmon-mcp-server")]
#[command(about = "MCP server that collects structured logs and exposes them to Claude")]
pub struct Cli {
    /// Session name (for named persistent sessions)
    #[arg(long)]
    pub session: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run as daemon (long-running log collector)
    Daemon {
        /// Port for both UDP and TCP GELF listeners
        #[arg(long, default_value = "12201")]
        gelf_port: u16,
        /// Override port for UDP only
        #[arg(long)]
        gelf_udp_port: Option<u16>,
        /// Override port for TCP only
        #[arg(long)]
        gelf_tcp_port: Option<u16>,
        /// Max log entries in ring buffer
        #[arg(long, default_value = "10000")]
        buffer_size: usize,
    },
}
