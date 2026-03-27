use clap::Parser;
use logmon_mcp_server::config::Config;
use logmon_mcp_server::engine::pipeline::LogPipeline;
use logmon_mcp_server::gelf::message::LogEntry;
use logmon_mcp_server::gelf::{udp, tcp};
use logmon_mcp_server::mcp::server::GelfMcpServer;
use logmon_mcp_server::mcp::notifications::spawn_notification_dispatcher;
use rmcp::ServiceExt;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize stderr logging (stdout is reserved for MCP JSON-RPC)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("logmon_mcp_server=info".parse()?)
        )
        .init();

    let config = Config::parse();

    let pipeline = Arc::new(LogPipeline::new(config.buffer_size));

    // Create a channel for GELF listeners to send parsed entries
    let (log_tx, mut log_rx) = tokio::sync::mpsc::channel::<LogEntry>(10_000);

    // Start GELF listeners
    let udp_addr = format!("0.0.0.0:{}", config.udp_port());
    let tcp_addr = format!("0.0.0.0:{}", config.tcp_port());

    let udp_handle = udp::start_udp_listener(&udp_addr, log_tx.clone()).await?;
    let tcp_handle = tcp::start_tcp_listener(&tcp_addr, log_tx).await?;

    eprintln!("GELF listeners started: UDP={}, TCP={}", udp_handle.port(), tcp_handle.port());

    // Spawn a task to receive log entries from GELF listeners and store them
    let pipeline_recv = pipeline.clone();
    tokio::spawn(async move {
        while let Some(mut entry) = log_rx.recv().await {
            entry.seq = pipeline_recv.assign_seq();
            pipeline_recv.append_to_store(entry);
        }
    });

    // Subscribe to pipeline events for notifications
    let event_rx = pipeline.subscribe_events();

    // Create and start MCP server on stdio
    let tcp_handle = Arc::new(tcp_handle);
    let server = GelfMcpServer::new(
        pipeline.clone(),
        udp_handle.port(),
        tcp_handle.port(),
        tcp_handle.clone(),
    );

    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let running = server.serve(transport).await?;

    // Wire up notification dispatcher with the peer handle.
    // running.peer() returns &Peer<RoleServer>; clone it for the dispatcher.
    spawn_notification_dispatcher(event_rx, running.peer().clone());

    eprintln!("MCP server running on stdio");
    running.waiting().await?;

    Ok(())
}
