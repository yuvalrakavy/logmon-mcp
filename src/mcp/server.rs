use crate::shim::bridge::DaemonBridge;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::ServerHandler;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone)]
pub struct GelfMcpServer {
    bridge: Arc<DaemonBridge>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl GelfMcpServer {
    pub fn new_with_bridge(bridge: Arc<DaemonBridge>) -> Self {
        Self {
            bridge,
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
    /// Number of entries before the anchor (default: 10)
    before: Option<u32>,
    /// Number of entries after the anchor (default: 10)
    after: Option<u32>,
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

#[derive(Deserialize, JsonSchema)]
struct DropSessionParams {
    /// Name of the session to drop
    name: String,
}

// ---- Tool router ----

#[rmcp::tool_router]
impl GelfMcpServer {
    #[rmcp::tool(description = "Get current server status including buffer sizes, trigger counts, connection info, and message statistics")]
    async fn get_status(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call("status.get", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    // ---- Log Query Tools ----

    #[rmcp::tool(description = "Get recent log entries from the buffer, newest first. Optionally filtered by a DSL expression.")]
    async fn get_recent_logs(
        &self,
        Parameters(p): Parameters<GetRecentLogsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call(
                "logs.recent",
                serde_json::json!({
                    "count": p.count,
                    "filter": p.filter,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Get log entries surrounding a specific entry identified by seq number. Returns context before and after.")]
    async fn get_log_context(
        &self,
        Parameters(p): Parameters<GetLogContextParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let seq = p.seq.ok_or_else(|| {
            rmcp::ErrorData::invalid_params(
                "seq is required in multi-session mode".to_string(),
                None,
            )
        })?;
        let result = self
            .bridge
            .call(
                "logs.context",
                serde_json::json!({
                    "seq": seq,
                    "before": p.before,
                    "after": p.after,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Export log entries to a file. Supports json or text format.")]
    async fn export_logs(
        &self,
        Parameters(p): Parameters<ExportLogsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let format = p.format.as_deref().unwrap_or("json");

        // Fetch logs from daemon
        let logs = self
            .bridge
            .call(
                "logs.export",
                serde_json::json!({
                    "count": p.count,
                    "filter": p.filter,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Format and write locally
        let empty = vec![];
        let entries = logs.as_array().unwrap_or(&empty);
        let entry_count = entries.len();

        let content = match format {
            "text" => entries
                .iter()
                .map(|e| {
                    format!(
                        "[{}] {} {} {}",
                        e.get("timestamp").and_then(|v| v.as_str()).unwrap_or("?"),
                        e.get("level").and_then(|v| v.as_str()).unwrap_or("?"),
                        e.get("host").and_then(|v| v.as_str()).unwrap_or("?"),
                        e.get("message").and_then(|v| v.as_str()).unwrap_or("?"),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => serde_json::to_string_pretty(&logs).map_err(|e| {
                rmcp::ErrorData::internal_error(format!("serialization error: {e}"), None)
            })?,
        };

        std::fs::write(&p.path, content).map_err(|e| {
            rmcp::ErrorData::internal_error(format!("failed to write file: {e}"), None)
        })?;

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
        let result = self
            .bridge
            .call("logs.clear", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    // ---- Filter Management Tools ----

    #[rmcp::tool(description = "List all buffer filters. Logs are stored only if they match at least one filter (OR semantics). If no filters are configured, all logs are stored.")]
    async fn get_filters(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call("filters.list", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Add a new buffer filter. Logs matching this filter will be stored. Uses OR semantics with existing filters.")]
    async fn add_filter(
        &self,
        Parameters(p): Parameters<AddFilterParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call(
                "filters.add",
                serde_json::json!({
                    "filter": p.filter,
                    "description": p.description,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Edit an existing buffer filter by ID.")]
    async fn edit_filter(
        &self,
        Parameters(p): Parameters<EditFilterParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call(
                "filters.edit",
                serde_json::json!({
                    "id": p.id,
                    "filter": p.filter,
                    "description": p.description,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Remove a buffer filter by ID.")]
    async fn remove_filter(
        &self,
        Parameters(p): Parameters<RemoveFilterParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call(
                "filters.remove",
                serde_json::json!({
                    "id": p.id,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    // ---- Trigger Management Tools ----

    #[rmcp::tool(description = "List all triggers. Triggers capture a window of logs around matching entries and emit notifications.")]
    async fn get_triggers(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call("triggers.list", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Add a new trigger. When a log matches the filter, the pre/post windows are captured and a notification is emitted.")]
    async fn add_trigger(
        &self,
        Parameters(p): Parameters<AddTriggerParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call(
                "triggers.add",
                serde_json::json!({
                    "filter": p.filter,
                    "pre_window": p.pre_window,
                    "post_window": p.post_window,
                    "notify_context": p.notify_context,
                    "description": p.description,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Edit an existing trigger by ID. Only the provided fields are updated.")]
    async fn edit_trigger(
        &self,
        Parameters(p): Parameters<EditTriggerParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call(
                "triggers.edit",
                serde_json::json!({
                    "id": p.id,
                    "filter": p.filter,
                    "pre_window": p.pre_window,
                    "post_window": p.post_window,
                    "notify_context": p.notify_context,
                    "description": p.description,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Remove a trigger by ID.")]
    async fn remove_trigger(
        &self,
        Parameters(p): Parameters<RemoveTriggerParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call(
                "triggers.remove",
                serde_json::json!({
                    "id": p.id,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    // ---- Session Management Tools ----

    #[rmcp::tool(description = "List all active sessions connected to the daemon.")]
    async fn get_sessions(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call("session.list", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Drop (disconnect) a session by name.")]
    async fn drop_session(
        &self,
        Parameters(p): Parameters<DropSessionParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call(
                "session.drop",
                serde_json::json!({
                    "name": p.name,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
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
