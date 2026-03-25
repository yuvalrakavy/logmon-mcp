use crate::engine::pipeline::LogPipeline;
use crate::gelf::tcp::TcpListenerHandle;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::ServerHandler;
use schemars::JsonSchema;
use serde::Deserialize;
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

// ---- Parameter structs ----

#[derive(Deserialize, JsonSchema)]
struct GetRecentLogsParams {
    /// Number of log entries to return (default: 100)
    count: Option<u32>,
    /// Optional DSL filter expression
    filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GetLogContextParams {
    /// Sequence number of the anchor entry
    seq: Option<u64>,
    /// RFC3339 timestamp of the anchor entry
    timestamp: Option<String>,
    /// Number of entries before the anchor (default: 10)
    before: Option<u32>,
    /// Number of entries after the anchor (default: 10)
    after: Option<u32>,
    /// Time window in seconds around the timestamp anchor (default: 5)
    window_secs: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct ExportLogsParams {
    /// File path to write logs to
    path: String,
    /// Maximum number of entries to export
    count: Option<u32>,
    /// Optional DSL filter expression
    filter: Option<String>,
    /// Output format: "json" or "text" (default: "json")
    format: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct AddFilterParams {
    /// DSL filter expression
    filter: String,
    /// Human-readable description
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct EditFilterParams {
    /// Filter ID to edit
    id: u32,
    /// New DSL filter expression
    filter: Option<String>,
    /// New description
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RemoveFilterParams {
    /// Filter ID to remove
    id: u32,
}

#[derive(Deserialize, JsonSchema)]
struct AddTriggerParams {
    /// DSL filter expression that activates the trigger
    filter: String,
    /// Number of messages to capture before the triggering event (default: 500)
    pre_window: Option<u32>,
    /// Number of messages to capture after the triggering event (default: 200)
    post_window: Option<u32>,
    /// Number of context entries to include in the notification (default: 5)
    notify_context: Option<u32>,
    /// Human-readable description
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct EditTriggerParams {
    /// Trigger ID to edit
    id: u32,
    /// New DSL filter expression
    filter: Option<String>,
    /// New pre-window size
    pre_window: Option<u32>,
    /// New post-window size
    post_window: Option<u32>,
    /// New notify-context size
    notify_context: Option<u32>,
    /// New description
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RemoveTriggerParams {
    /// Trigger ID to remove
    id: u32,
}

// ---- Tool router ----

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

    // ---- Log Query Tools ----

    #[rmcp::tool(description = "Get recent log entries from the buffer, newest first. Optionally filtered by a DSL expression.")]
    async fn get_recent_logs(
        &self,
        Parameters(p): Parameters<GetRecentLogsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let count = p.count.unwrap_or(100) as usize;
        let entries = self.pipeline.recent_logs(count, p.filter.as_deref());
        let json = serde_json::to_string_pretty(&entries)
            .map_err(|e| rmcp::ErrorData::internal_error(format!("serialization error: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[rmcp::tool(description = "Get log entries surrounding a specific entry identified by seq number or timestamp. Returns context before and after.")]
    async fn get_log_context(
        &self,
        Parameters(p): Parameters<GetLogContextParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let before = p.before.unwrap_or(10) as usize;
        let after = p.after.unwrap_or(10) as usize;
        let window_secs = p.window_secs.unwrap_or(5);

        let entries = if let Some(seq) = p.seq {
            self.pipeline.context_by_seq(seq, before, after)
        } else if let Some(ts_str) = p.timestamp {
            let timestamp = chrono::DateTime::parse_from_rfc3339(&ts_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| rmcp::ErrorData::invalid_params(format!("invalid timestamp: {e}"), None))?;
            let window = std::time::Duration::from_secs(window_secs as u64);
            self.pipeline.context_by_time(timestamp, window)
        } else {
            return Err(rmcp::ErrorData::invalid_params(
                "either seq or timestamp must be provided".to_string(),
                None,
            ));
        };

        let json = serde_json::to_string_pretty(&entries)
            .map_err(|e| rmcp::ErrorData::internal_error(format!("serialization error: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[rmcp::tool(description = "Export log entries to a file. Supports json or text format.")]
    async fn export_logs(
        &self,
        Parameters(p): Parameters<ExportLogsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let count = p.count.unwrap_or(u32::MAX) as usize;
        let format = p.format.as_deref().unwrap_or("json");
        let entries = self.pipeline.recent_logs(count, p.filter.as_deref());
        let entry_count = entries.len();

        let content = match format {
            "text" => {
                entries
                    .iter()
                    .map(|e| format!("[{}] {} {} {}", e.timestamp, e.level, e.host, e.message))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            _ => serde_json::to_string_pretty(&entries)
                .map_err(|e| rmcp::ErrorData::internal_error(format!("serialization error: {e}"), None))?,
        };

        std::fs::write(&p.path, content)
            .map_err(|e| rmcp::ErrorData::internal_error(format!("failed to write file: {e}"), None))?;

        let result = serde_json::json!({
            "exported": entry_count,
            "path": p.path,
            "format": format,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Clear all log entries from the in-memory buffer.")]
    async fn clear_logs(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let cleared = self.pipeline.clear_logs();
        let result = serde_json::json!({ "cleared": cleared });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    // ---- Filter Management Tools ----

    #[rmcp::tool(description = "List all buffer filters. Logs are stored only if they match at least one filter (OR semantics). If no filters are configured, all logs are stored.")]
    async fn get_filters(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let filters = self.pipeline.list_filters();
        let json_filters: Vec<serde_json::Value> = filters
            .iter()
            .map(|f| {
                serde_json::json!({
                    "id": f.id,
                    "filter": f.filter_string,
                    "description": f.description,
                })
            })
            .collect();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json_filters).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Add a new buffer filter. Logs matching this filter will be stored. Uses OR semantics with existing filters.")]
    async fn add_filter(
        &self,
        Parameters(p): Parameters<AddFilterParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let id = self
            .pipeline
            .add_filter(&p.filter, p.description.as_deref())
            .map_err(|e| rmcp::ErrorData::invalid_params(format!("invalid filter: {e}"), None))?;
        let result = serde_json::json!({
            "id": id,
            "filter": p.filter,
            "description": p.description,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Edit an existing buffer filter by ID.")]
    async fn edit_filter(
        &self,
        Parameters(p): Parameters<EditFilterParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let info = self
            .pipeline
            .edit_filter(p.id, p.filter.as_deref(), p.description.as_deref())
            .map_err(|e| rmcp::ErrorData::invalid_params(format!("{e}"), None))?;
        let result = serde_json::json!({
            "id": info.id,
            "filter": info.filter_string,
            "description": info.description,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Remove a buffer filter by ID.")]
    async fn remove_filter(
        &self,
        Parameters(p): Parameters<RemoveFilterParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.pipeline
            .remove_filter(p.id)
            .map_err(|e| rmcp::ErrorData::invalid_params(format!("{e}"), None))?;
        let result = serde_json::json!({ "removed": p.id });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    // ---- Trigger Management Tools ----

    #[rmcp::tool(description = "List all triggers. Triggers capture a window of logs around matching entries and emit notifications.")]
    async fn get_triggers(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let triggers = self.pipeline.list_triggers();
        let json_triggers: Vec<serde_json::Value> = triggers
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "filter": t.filter_string,
                    "pre_window": t.pre_window,
                    "post_window": t.post_window,
                    "notify_context": t.notify_context,
                    "description": t.description,
                    "match_count": t.match_count,
                })
            })
            .collect();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json_triggers).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Add a new trigger. When a log matches the filter, the pre/post windows are captured and a notification is emitted.")]
    async fn add_trigger(
        &self,
        Parameters(p): Parameters<AddTriggerParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let pre_window = p.pre_window.unwrap_or(500);
        let post_window = p.post_window.unwrap_or(200);
        let notify_context = p.notify_context.unwrap_or(5);
        let id = self
            .pipeline
            .add_trigger(&p.filter, pre_window, post_window, notify_context, p.description.as_deref())
            .map_err(|e| rmcp::ErrorData::invalid_params(format!("invalid trigger: {e}"), None))?;
        let result = serde_json::json!({
            "id": id,
            "filter": p.filter,
            "pre_window": pre_window,
            "post_window": post_window,
            "notify_context": notify_context,
            "description": p.description,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Edit an existing trigger by ID. Only the provided fields are updated.")]
    async fn edit_trigger(
        &self,
        Parameters(p): Parameters<EditTriggerParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let info = self
            .pipeline
            .edit_trigger(
                p.id,
                p.filter.as_deref(),
                p.pre_window,
                p.post_window,
                p.notify_context,
                p.description.as_deref(),
            )
            .map_err(|e| rmcp::ErrorData::invalid_params(format!("{e}"), None))?;
        let result = serde_json::json!({
            "id": info.id,
            "filter": info.filter_string,
            "pre_window": info.pre_window,
            "post_window": info.post_window,
            "notify_context": info.notify_context,
            "description": info.description,
            "match_count": info.match_count,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Remove a trigger by ID.")]
    async fn remove_trigger(
        &self,
        Parameters(p): Parameters<RemoveTriggerParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.pipeline
            .remove_trigger(p.id)
            .map_err(|e| rmcp::ErrorData::invalid_params(format!("{e}"), None))?;
        let result = serde_json::json!({ "removed": p.id });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }
}

#[rmcp::tool_handler]
impl ServerHandler for GelfMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}
