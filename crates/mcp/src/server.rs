use logmon_broker_sdk::Broker;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::ServerHandler;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Clone)]
pub struct GelfMcpServer {
    broker: Broker,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl GelfMcpServer {
    pub fn new(broker: Broker) -> Self {
        Self {
            broker,
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
    /// Filter logs by trace ID (32-char hex)
    trace_id: Option<String>,
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

#[derive(Deserialize, JsonSchema)]
struct AddBookmarkParams {
    /// Bookmark name (alphanumerics, '-', '_'; max 64 chars). Will be qualified
    /// with the calling session's name automatically.
    name: String,
    /// If true, overwrite an existing bookmark with the same qualified name.
    replace: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct ListBookmarksParams {
    /// Optional: filter to bookmarks created by this session name.
    session: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RemoveBookmarkParams {
    /// Bare name (resolved against current session) or qualified "session/name".
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct ClearBookmarksParams {
    /// Optional session name to clear. Defaults to the calling session.
    /// Use to clear another session's bookmarks (no nuclear "clear all" — call
    /// once per session if you want to wipe multiple).
    session: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GetRecentTracesParams {
    /// Max traces to return (default: 20)
    count: Option<u32>,
    /// Span filter DSL expression
    filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GetTraceParams {
    /// 32-character hex trace ID
    trace_id: String,
    /// Include linked logs (default: true)
    include_logs: Option<bool>,
    /// Filter spans within the trace
    filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GetTraceSummaryParams {
    /// 32-character hex trace ID
    trace_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct GetSlowSpansParams {
    /// Duration threshold in milliseconds (default: 100)
    min_duration_ms: Option<f64>,
    /// Max results (default: 20)
    count: Option<u32>,
    /// Additional span filter
    filter: Option<String>,
    /// Group results by "name" or "service"
    group_by: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GetSpanContextParams {
    /// Span sequence number
    seq: u64,
    /// Spans before (default: 5)
    before: Option<u32>,
    /// Spans after (default: 5)
    after: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct GetTraceLogsParams {
    /// 32-character hex trace ID
    trace_id: String,
    /// Additional log filter DSL
    filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct CreateDomainParams {
    /// Domain name — the isolation key.
    name: String,
    /// GELF UDP+TCP port. Omitted → auto-allocate; 0 → disable GELF for this domain.
    gelf_port: Option<u16>,
    /// OTLP gRPC port. Omitted → auto-allocate; 0 → disable.
    otlp_grpc_port: Option<u16>,
    /// OTLP HTTP port. Omitted → auto-allocate; 0 → disable.
    otlp_http_port: Option<u16>,
    /// Log ring capacity (default: the daemon's configured size).
    log_buffer_size: Option<usize>,
    /// Span ring capacity (default: the daemon's configured size).
    span_buffer_size: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct DeleteDomainParams {
    /// Name of the domain to delete. Refuses config-declared domains (incl. 'default').
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct RenameSessionParams {
    /// New session name: <Project>-Main-<short8> or <Project>-tN-<branch>
    /// ('/' sanitized to '-'; alphanumerics, '-' and '_' only).
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct UseDomainParams {
    /// Domain to bind this session to for subsequent queries and notifications.
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct AddCollectorParams {
    /// Collector name. Letters, digits, '_' and '-' only.
    name: String,
    /// Span filter DSL. Matched against every span in the domain this session
    /// is bound to *now*; the collector stays pinned to that domain afterwards.
    filter: String,
    /// "scalar" (counts and totals only), "timing" (adds percentiles, wall
    /// union, warm-up), or "tree" (adds self time, nesting and call paths).
    /// Default "tree".
    level: Option<String>,
    /// Span attributes to split the numbers by, e.g. ["cache.enabled"] for an
    /// A/B. Values are read directly, so booleans and numbers work.
    group_keys: Option<Vec<String>>,
    /// Why this collector exists. Returned with every read, so a result found
    /// later still carries its context.
    description: Option<String>,
    /// Per-collector retained-sample budget in bytes (default 64 MiB).
    max_sample_bytes: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct CollectorNameParams {
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct GetCollectorParams {
    name: String,
    /// Read a recorded run by its label instead of the live window.
    snapshot: Option<String>,
    /// Break the numbers down by "name", "group", "trace" or "path".
    group_by: Option<String>,
    /// Exclude spans starting within this many ms of the first matched span.
    skip_warmup_ms: Option<f64>,
    /// Rows returned in the breakdown (default 20).
    top_n: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct EditCollectorParams {
    name: String,
    /// Free — changes nothing about what is collected.
    description: Option<String>,
    /// Any of the fields below DISCARDS the live window. Recorded snapshots are
    /// untouched and each keeps the definition it was taken under.
    filter: Option<String>,
    /// scalar | timing | tree. Permitted in both directions — dropping a
    /// `tree` collector to `timing` buys 2.5x the retained records, which is
    /// the only remedy left once the sample budget is exhausted.
    level: Option<String>,
    group_keys: Option<Vec<String>>,
    max_sample_bytes: Option<u64>,
    /// Re-pin to another domain, only while zeroed. The remedy for a collector
    /// orphaned by a restart.
    domain: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct SnapshotCollectorParams {
    name: String,
    /// Unique per collector. Omitted → "snapshot-<n>", and n never repeats.
    label: Option<String>,
    /// What this run was. Recorded with the data, so a number found weeks
    /// later still says what it measured. Give one.
    description: Option<String>,
    /// Provenance logmon cannot infer — commit, build profile, config.
    meta: Option<serde_json::Value>,
    /// Zero the collector and start the next window. Default true.
    reset: Option<bool>,
    /// Store the sample-derived figures (self time, percentiles). Computed now
    /// or never — the samples themselves are not retained. Default true.
    projections: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct CollectorHistoryParams {
    name: String,
    /// Most recent N runs. Omitted → all retained.
    limit: Option<u32>,
    /// Also combine them: exact totals and percentiles add across runs, and a
    /// run-to-run spread is reported. Sample-derived figures do not merge.
    merge: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct ProfileTracesParams {
    /// Span filter DSL. Default "ALL".
    filter: Option<String>,
    /// Break the numbers down by "name", "group", "trace" or "path".
    group_by: Option<String>,
    /// Span attributes to split by. Required for group_by "group".
    group_keys: Option<Vec<String>>,
    /// Exclude spans starting within this many ms of the first matched span.
    skip_warmup_ms: Option<f64>,
    /// Rows returned in the breakdown (default 20).
    top_n: Option<u32>,
}

// ---- Tool router ----

#[rmcp::tool_router]
impl GelfMcpServer {
    #[rmcp::tool(
        description = "Get current server status including buffer sizes, trigger counts, connection info, and message statistics"
    )]
    async fn get_status(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call("status.get", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    // ---- Log Query Tools ----

    #[rmcp::tool(
        description = "Get recent log entries from the buffer, newest first. Optionally filtered by a DSL expression. Response carries matched/scanned/buffer_total diagnostics (scanned=0 = empty buffer; matched=0 with scanned>0 = filter matched nothing while data flows) plus truncated/evicted_before_window when a bookmark/cursor window rolled off. Unknown-selector comparison typos like level>=WARN are rejected with a suggestion."
    )]
    async fn get_recent_logs(
        &self,
        Parameters(p): Parameters<GetRecentLogsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "logs.recent",
                serde_json::json!({
                    "count": p.count,
                    "filter": p.filter,
                    "trace_id": p.trace_id,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Get log entries surrounding a specific entry identified by seq number. Returns context before and after."
    )]
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
            .broker
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
            .broker
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
            .broker
            .call("logs.clear", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    // ---- Filter Management Tools ----

    #[rmcp::tool(
        description = "List all buffer filters. Logs are stored only if they match at least one filter (OR semantics). If no filters are configured, all logs are stored."
    )]
    async fn get_filters(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call("filters.list", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Add a new buffer filter. Logs matching this filter will be stored. Uses OR semantics with existing filters."
    )]
    async fn add_filter(
        &self,
        Parameters(p): Parameters<AddFilterParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
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
            .broker
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
            .broker
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

    #[rmcp::tool(
        description = "List all triggers. Triggers capture a window of logs around matching entries and emit notifications."
    )]
    async fn get_triggers(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call("triggers.list", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Add a new trigger. When a log matches the filter, the pre/post windows are captured and a notification is emitted."
    )]
    async fn add_trigger(
        &self,
        Parameters(p): Parameters<AddTriggerParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
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

    #[rmcp::tool(
        description = "Edit an existing trigger by ID. Only the provided fields are updated."
    )]
    async fn edit_trigger(
        &self,
        Parameters(p): Parameters<EditTriggerParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
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
            .broker
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

    // ---- Trace Tools ----

    #[rmcp::tool(description = "List recent traces with timing and error info")]
    async fn get_recent_traces(
        &self,
        Parameters(p): Parameters<GetRecentTracesParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "traces.recent",
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

    #[rmcp::tool(description = "Get full trace detail — span tree + linked logs")]
    async fn get_trace(
        &self,
        Parameters(p): Parameters<GetTraceParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "traces.get",
                serde_json::json!({
                    "trace_id": p.trace_id,
                    "include_logs": p.include_logs,
                    "filter": p.filter,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Compact timing breakdown highlighting bottlenecks")]
    async fn get_trace_summary(
        &self,
        Parameters(p): Parameters<GetTraceSummaryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "traces.summary",
                serde_json::json!({
                    "trace_id": p.trace_id,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Find slow spans, optionally grouped by operation name")]
    async fn get_slow_spans(
        &self,
        Parameters(p): Parameters<GetSlowSpansParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "traces.slow",
                serde_json::json!({
                    "min_duration_ms": p.min_duration_ms,
                    "count": p.count,
                    "filter": p.filter,
                    "group_by": p.group_by,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Arm a span time collector. Accumulates exact totals, percentiles and \
                       (at tree level) self time for every span matching the filter, from now \
                       until reset or removed. Use for before/after measurement: arm, run the \
                       workload, read, reset, change one thing, run again."
    )]
    async fn add_collector(
        &self,
        Parameters(p): Parameters<AddCollectorParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "collectors.add",
                serde_json::json!({
                    "name": p.name,
                    "filter": p.filter,
                    "level": p.level,
                    "group_keys": p.group_keys,
                    "description": p.description,
                    "max_sample_bytes": p.max_sample_bytes,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "List this session's collectors and what each has matched so far")]
    async fn list_collectors(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call("collectors.list", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Read a collector's numbers. Returns exact totals, sketch percentiles, \
                       and sample-derived figures separately — they cover different \
                       populations and any field that cannot be computed says why."
    )]
    async fn get_collector(
        &self,
        Parameters(p): Parameters<GetCollectorParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "collectors.get",
                serde_json::json!({
                    "name": p.name,
                    "snapshot": p.snapshot,
                    "group_by": p.group_by,
                    "skip_warmup_ms": p.skip_warmup_ms,
                    "top_n": p.top_n,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Change an armed collector. Editing only the description changes \
                       nothing else; editing the filter, level, group_keys, max_sample_bytes \
                       or domain DISCARDS the live window, because a window and the \
                       definition describing it must not disagree. Recorded snapshots are \
                       never touched. Use it to re-pin a collector orphaned by a restart, or \
                       to drop a level when the sample budget runs out."
    )]
    async fn edit_collector(
        &self,
        Parameters(p): Parameters<EditCollectorParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "collectors.edit",
                serde_json::json!({
                    "name": p.name,
                    "description": p.description,
                    "filter": p.filter,
                    "level": p.level,
                    "group_keys": p.group_keys,
                    "max_sample_bytes": p.max_sample_bytes,
                    "domain": p.domain,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Record the current window as a named run and start the next one. This \
                       is the between-runs move for a before/after comparison: arm, run A, \
                       snapshot, change one thing, run B, snapshot, then compare. Unlike \
                       reset_collector it KEEPS the run. Always pass a description."
    )]
    async fn snapshot_collector(
        &self,
        Parameters(p): Parameters<SnapshotCollectorParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "collectors.snapshot",
                serde_json::json!({
                    "name": p.name,
                    "label": p.label,
                    "description": p.description,
                    "meta": p.meta,
                    "reset": p.reset,
                    "projections": p.projections,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "List a collector's recorded runs, oldest first, each with the \
                       definition it was taken under. With merge=true also combines them and \
                       reports the run-to-run spread — which is what tells you whether a \
                       difference between two runs is real or just noise."
    )]
    async fn get_collector_history(
        &self,
        Parameters(p): Parameters<CollectorHistoryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "collectors.history",
                serde_json::json!({
                    "name": p.name,
                    "limit": p.limit,
                    "merge": p.merge,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Zero a collector and start a fresh window, keeping it armed. DISCARDS \
                       the run — use snapshot_collector if you want to keep it. Returns a \
                       summary of what was thrown away."
    )]
    async fn reset_collector(
        &self,
        Parameters(p): Parameters<CollectorNameParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call("collectors.reset", serde_json::json!({ "name": p.name }))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Remove a collector and release its sample budget")]
    async fn remove_collector(
        &self,
        Parameters(p): Parameters<CollectorNameParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call("collectors.remove", serde_json::json!({ "name": p.name }))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Profile spans already in the buffer, without arming anything. Same \
                       numbers as get_collector but over what is stored now — use it to look \
                       back at a run that already happened, and a collector to measure one \
                       that has not started."
    )]
    async fn profile_traces(
        &self,
        Parameters(p): Parameters<ProfileTracesParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "traces.profile",
                serde_json::json!({
                    "filter": p.filter,
                    "group_by": p.group_by,
                    "group_keys": p.group_keys,
                    "skip_warmup_ms": p.skip_warmup_ms,
                    "top_n": p.top_n,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Get spans surrounding a specific span in time")]
    async fn get_span_context(
        &self,
        Parameters(p): Parameters<GetSpanContextParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "spans.context",
                serde_json::json!({
                    "seq": p.seq,
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

    #[rmcp::tool(description = "Get all logs linked to a trace")]
    async fn get_trace_logs(
        &self,
        Parameters(p): Parameters<GetTraceLogsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "traces.logs",
                serde_json::json!({
                    "trace_id": p.trace_id,
                    "filter": p.filter,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    // ---- Bookmark Tools ----

    #[rmcp::tool(
        description = "Set a named bookmark at the current moment. Bookmarks are timestamps usable in filter DSL via b>=name / b<=name. Use them to scope queries to a range without destructively clearing logs."
    )]
    async fn add_bookmark(
        &self,
        Parameters(p): Parameters<AddBookmarkParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "bookmarks.add",
                serde_json::json!({
                    "name": p.name,
                    "replace": p.replace,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "List all live bookmarks across all sessions, newest first. Optionally filter by session name."
    )]
    async fn list_bookmarks(
        &self,
        Parameters(p): Parameters<ListBookmarksParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "bookmarks.list",
                serde_json::json!({ "session": p.session }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Remove a bookmark by name. Bare name resolves to the current session; use 'session/name' to remove a bookmark from another session."
    )]
    async fn remove_bookmark(
        &self,
        Parameters(p): Parameters<RemoveBookmarkParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call("bookmarks.remove", serde_json::json!({ "name": p.name }))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Clear all bookmarks for a session at once. Defaults to the calling session. Useful for iterative debugging workflows: wipe all bookmarks, re-add fresh ones, repeat. Pass an explicit session name to clear another session's bookmarks."
    )]
    async fn clear_bookmarks(
        &self,
        Parameters(p): Parameters<ClearBookmarksParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "bookmarks.clear",
                serde_json::json!({ "session": p.session }),
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
            .broker
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
            .broker
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

    // ---- Domain Management Tools ----

    #[rmcp::tool(
        description = "Create (or idempotently ensure) an isolated domain — a full broker instance with its own log/span buffers, receivers, and triggers. Omitted ports are auto-allocated; a port of 0 disables that receiver. Ephemeral (gone on daemon restart)."
    )]
    async fn create_domain(
        &self,
        Parameters(p): Parameters<CreateDomainParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "domains.create",
                serde_json::json!({
                    "name": p.name,
                    "gelf_port": p.gelf_port,
                    "otlp_grpc_port": p.otlp_grpc_port,
                    "otlp_http_port": p.otlp_http_port,
                    "log_buffer_size": p.log_buffer_size,
                    "span_buffer_size": p.span_buffer_size,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Delete a domain and tear down its receivers. Refuses config-declared domains including 'default'."
    )]
    async fn delete_domain(
        &self,
        Parameters(p): Parameters<DeleteDomainParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call("domains.delete", serde_json::json!({ "name": p.name }))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "List all live domains with their ports, source (config/persistent/ephemeral), and log/span counts."
    )]
    async fn list_domains(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call("domains.list", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Bind this session to a domain. Subsequent log/trace queries and trigger notifications target that domain until you switch again. Errors if the domain does not exist."
    )]
    async fn use_domain(
        &self,
        Parameters(p): Parameters<UseDomainParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call("domains.use", serde_json::json!({ "name": p.name }))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Rename this logmon session to a meaningful name (convention: <Project>-Main-<short8> for a home/main conversation, <Project>-tN-<branch> after claiming a dev-track lane; sanitize '/' to '-'). Preserves all session state. ERRORS with 'already connected' when the target name is held by a LIVE session — that means another conversation is already working that dev-track: STOP rather than fight over it. A stale (disconnected) holder is displaced automatically."
    )]
    async fn rename_session(
        &self,
        Parameters(p): Parameters<RenameSessionParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call("session.rename", serde_json::json!({ "name": p.name }))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Dispose the bound domain's data — logs and spans — keeping the domain and its receivers alive. Sequence numbers stay monotonic. (logs.clear is the logs-only cousin.)"
    )]
    async fn clear_domain(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call("domains.clear", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }
}

/// Skill content shipped as MCP `instructions` so any compliant client
/// (Claude Code, Cursor, etc.) surfaces it as server-level guidance
/// without the user installing the file by hand. The file lives at
/// `skill/logmon.md` at the workspace root and is embedded at compile
/// time.
const SKILL_INSTRUCTIONS: &str = include_str!("../../../skill/logmon.md");

#[rmcp::tool_handler]
impl ServerHandler for GelfMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(SKILL_INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::SKILL_INSTRUCTIONS;

    #[test]
    fn skill_instructions_is_embedded_and_non_empty() {
        // Catches `skill/logmon.md` going missing, getting truncated, or
        // losing its YAML frontmatter at compile/test time rather than at
        // runtime in a client's session.
        assert!(
            !SKILL_INSTRUCTIONS.is_empty(),
            "skill/logmon.md is empty — embedding failed"
        );
        assert!(
            SKILL_INSTRUCTIONS.starts_with("---\n"),
            "skill/logmon.md must start with YAML frontmatter"
        );
        assert!(
            SKILL_INSTRUCTIONS.contains("name: logmon"),
            "skill/logmon.md frontmatter must declare `name: logmon`"
        );
        assert!(
            SKILL_INSTRUCTIONS.contains("## When to reach for logmon"),
            "skill/logmon.md must contain the `When to reach for logmon` section"
        );
    }
}
