use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "logmon-mcp-server")]
#[command(about = "MCP server that collects structured logs and exposes them to Claude")]
pub struct Config {
    /// Port for both UDP and TCP listeners
    #[arg(long, default_value = "12201", env = "GELF_PORT")]
    pub port: u16,

    /// Override port for UDP only
    #[arg(long, env = "GELF_UDP_PORT")]
    pub udp_port: Option<u16>,

    /// Override port for TCP only
    #[arg(long, env = "GELF_TCP_PORT")]
    pub tcp_port: Option<u16>,

    /// Max log entries in ring buffer
    #[arg(long, default_value = "10000", env = "GELF_BUFFER_SIZE")]
    pub buffer_size: usize,
}

impl Config {
    pub fn udp_port(&self) -> u16 {
        self.udp_port.unwrap_or(self.port)
    }
    pub fn tcp_port(&self) -> u16 {
        self.tcp_port.unwrap_or(self.port)
    }
}
