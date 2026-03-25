use crate::engine::pipeline::LogPipeline;
use crate::gelf::tcp::TcpListenerHandle;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::*;
use rmcp::ServerHandler;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
pub struct GelfMcpServer {
    pub(crate) pipeline: Arc<LogPipeline>,
    pub(crate) udp_port: u16,
    pub(crate) tcp_port: u16,
    pub(crate) tcp_handle: Arc<TcpListenerHandle>,
    pub(crate) start_time: Instant,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl GelfMcpServer {
    pub fn new(
        pipeline: Arc<LogPipeline>,
        udp_port: u16,
        tcp_port: u16,
        tcp_handle: Arc<TcpListenerHandle>,
    ) -> Self {
        Self {
            pipeline,
            udp_port,
            tcp_port,
            tcp_handle,
            start_time: Instant::now(),
            tool_router: Self::tool_router(),
        }
    }
}

#[rmcp::tool_router]
impl GelfMcpServer {
    #[rmcp::tool(description = "Get current server status including buffer sizes, trigger counts, connection info, and message statistics")]
    async fn get_status(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let stats = self.pipeline.store_stats();
        let triggers = self.pipeline.list_triggers();
        let filters = self.pipeline.list_filters();

        let status = serde_json::json!({
            "buffer_size": self.pipeline.store_len(),
            "filter_count": filters.len(),
            "trigger_count": triggers.len(),
            "udp_port": self.udp_port,
            "tcp_port": self.tcp_port,
            "connected_tcp_clients": self.tcp_handle.connected_clients(),
            "uptime_secs": self.start_time.elapsed().as_secs(),
            "total_received": stats.total_received,
            "total_stored": stats.total_stored,
            "malformed_count": stats.malformed_count,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&status).unwrap(),
        )]))
    }
}

#[rmcp::tool_handler]
impl ServerHandler for GelfMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}
