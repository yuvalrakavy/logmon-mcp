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
#[serde(deny_unknown_fields)]
struct GetRecentLogsParams {
    /// Number of log entries to return (default: 50)
    count: Option<u32>,
    /// Optional DSL filter expression
    filter: Option<String>,
    /// Filter logs by trace ID (32-char hex)
    trace_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetLogContextParams {
    /// Sequence number of the anchor entry
    seq: Option<u64>,
    /// Number of entries before the anchor (default: 10)
    before: Option<u32>,
    /// Number of entries after the anchor (default: 10)
    after: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct AddFilterParams {
    /// DSL filter expression
    filter: String,
    /// Human-readable description
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EditFilterParams {
    /// Filter ID to edit
    id: u32,
    /// New DSL filter expression
    filter: Option<String>,
    /// New description
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveFilterParams {
    /// Filter ID to remove
    id: u32,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// Fire once, then disarm. Default `false` — the trigger stays armed.
    ///
    /// The daemon has honoured this since the trigger surface shipped and the
    /// skill has documented it; the shim simply had no field for it, so the key
    /// was dropped by serde and a caller asking for one-shot got a permanent
    /// trigger, reported as success.
    oneshot: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// Set or clear fire-once. Absent leaves it as it is.
    oneshot: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveTriggerParams {
    /// Trigger ID to remove
    id: u32,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DropSessionParams {
    /// Name of the session to drop
    name: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddBookmarkParams {
    /// Bookmark name (alphanumerics, '-', '_'; max 64 chars). Will be qualified
    /// with the calling session's name automatically.
    name: String,
    /// If true, overwrite an existing bookmark with the same qualified name.
    replace: Option<bool>,
    /// Anchor the bookmark at this seq instead of the current position.
    ///
    /// The point of a bookmark is usually "from here on", so the default is the
    /// daemon's current seq. This is for the other case: marking a boundary you
    /// have already passed, from a seq another query returned.
    start_seq: Option<u64>,
    /// What this bookmark marks. Recorded with it, so a name found weeks later
    /// still says what it was for.
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListBookmarksParams {
    /// Optional: filter to bookmarks created by this session name.
    session: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveBookmarkParams {
    /// Bare name (resolved against current session) or qualified "session/name".
    name: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ClearBookmarksParams {
    /// Optional session name to clear. Defaults to the calling session.
    /// Use to clear another session's bookmarks (no nuclear "clear all" — call
    /// once per session if you want to wipe multiple).
    session: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetRecentTracesParams {
    /// Max traces to return (default: 20)
    count: Option<u32>,
    /// Span filter DSL expression
    filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetTraceParams {
    /// 32-character hex trace ID
    trace_id: String,
    /// Include linked logs (default: true)
    include_logs: Option<bool>,
    /// Filter spans within the trace
    filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetTraceSummaryParams {
    /// 32-character hex trace ID
    trace_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct GetSpanContextParams {
    /// Span sequence number
    seq: u64,
    /// Spans before (default: 5)
    before: Option<u32>,
    /// Spans after (default: 5)
    after: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetTraceLogsParams {
    /// 32-character hex trace ID
    trace_id: String,
    /// Additional log filter DSL
    filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct DeleteDomainParams {
    /// Name of the domain to delete. Refuses config-declared domains (incl. 'default').
    name: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RenameSessionParams {
    /// New session name: <Project>-Main-<short8> or <Project>-tN-<branch>
    /// ('/' sanitized to '-'; alphanumerics, '-' and '_' only).
    name: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UseDomainParams {
    /// Domain to bind this session to for subsequent queries and notifications.
    name: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateDomainDataParams {
    /// Entries to record. Each is `{path, value?, ttl?}`.
    ///
    /// **Value present** → set it. **Value absent** → *validate*: confirm what
    /// is already there, moving only its confirmation time. A key-only entry
    /// never creates — recording a key with no value would be a guess.
    ///
    /// `ttl` is `30s` / `5m` / `2h` / `7d` / `4w`, or `false` to clear one.
    /// Absent leaves any existing lifetime alone, exactly as an absent value
    /// does, so restating a value cannot silently drop its lifetime.
    entries: Vec<serde_json::Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetDomainDataParams {
    /// Narrow to a subtree, matched on segment boundaries: `/Versions` returns
    /// `/Versions/*`, `/Ver` returns nothing.
    #[serde(default)]
    prefix: Option<String>,
    /// Return only keys last confirmed longer ago than this — the "what has
    /// gone stale" query.
    #[serde(default)]
    validated_before_secs: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveDomainDataParams {
    /// Prefix patterns, matched on segment boundaries. **There is no undo.**
    patterns: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// A rolling guard: tell me when this metric crosses this value over this
    /// window. Read the verdict back from list_collectors or get_collector.
    threshold: Option<ThresholdParams>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CollectorNameParams {
    name: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    /// Set or replace the rolling guard; send JSON `null` to remove one.
    ///
    /// **Doubly optional, and it has to be.** "Leave it alone" and "take it
    /// away" are different requests and one `Option` cannot express both — the
    /// daemon's own wire type is `Option<Option<ThresholdSpec>>`
    /// (`protocol/src/methods.rs:1068`), the only tri-state parameter in the
    /// protocol. `deserialize_some` is what makes an explicitly-null field
    /// arrive as `Some(None)` rather than collapsing into the absent case.
    ///
    /// Shape: `{ metric, op, value, window_ms, group? }`. Validated by the
    /// daemon, which is the only party that knows whether `group` names a key
    /// this collector actually carries.
    #[serde(default, deserialize_with = "deserialize_some")]
    threshold: Option<Option<serde_json::Value>>,
}

/// Distinguish an absent field from one explicitly set to `null`.
///
/// `Option<T>` collapses both to `None`. Wrapping in a second `Option` and
/// deserializing the inner one unconditionally makes absent → `None` and
/// `null` → `Some(None)`. Only `collectors.edit`'s `threshold` needs this, and
/// it needs it because the difference is the difference between leaving a guard
/// armed and removing it.
fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// Keep the per-span-name breakdown in the recorded run. Default true.
    ///
    /// Per-axis breakdowns are what a later comparison splits by, and they are
    /// dropped on the way to disk when this is false — so the choice is made
    /// here, once, and cannot be revisited from a recorded run.
    per_name: Option<bool>,
    /// Keep the per-group-key breakdown in the recorded run. Default true.
    per_group: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CollectorHistoryParams {
    name: String,
    /// Most recent N runs. Omitted → all retained.
    limit: Option<u32>,
    /// Also combine them: exact totals and percentiles add across runs, and a
    /// run-to-run spread is reported. Sample-derived figures do not merge.
    merge: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ThresholdParams {
    /// "count", "total_ms", "avg_ms", "error_count" or "error_rate_pct".
    /// Percentiles are NOT available: a rolling percentile needs a duration
    /// sketch per bucket, and per-collector memory is bounded on purpose. Use
    /// "avg_ms" for the guard and get_collector for real percentiles.
    metric: String,
    /// Restrict to one group value. Needs the collector to declare group_keys.
    group: Option<String>,
    /// "gt", "gte", "lt" or "lte". A downward op detects a drop WHILE traffic
    /// continues; it does nothing if traffic stops, because the window advances
    /// on span arrival rather than on a clock.
    op: String,
    value: f64,
    /// The rolling window, 16..=600000 ms.
    window_ms: u64,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CollectorDiffParams {
    /// Baseline arm: "<collector>" for the live window, "<collector>@<label>"
    /// for one recorded run, or "<collector>@*" for every recorded run merged.
    a: String,
    /// The arm to compare against the baseline, same syntax.
    b: String,
    /// Break the deltas down by "name" or "group". Not "trace" or "path" — a
    /// recorded run does not retain the per-span records those need.
    group_by: Option<String>,
    /// Rows returned in the breakdown (default 20).
    top_n: Option<u32>,
    /// Compare even though the arms matched different span populations.
    allow_mismatch: Option<bool>,
    /// Compare even though one arm lost spans and the other did not.
    allow_lossy: Option<bool>,
    /// Compare even though both arms hit the sample budget.
    allow_truncated: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CollectorDocumentParams {
    /// The arms to document. The FIRST is the baseline; every other is compared
    /// against it. Same syntax as diff_collectors: "<collector>",
    /// "<collector>@<label>", or "<collector>@*" for every recorded run merged.
    names: Vec<String>,
    /// "md" (default), "json", or "folded" for a flame graph.
    format: Option<String>,
    /// What you were trying to find out. Supply it — months later this is what
    /// makes the document triageable.
    question: Option<String>,
    /// What you concluded. Normally supplied on a SECOND call, after reading the
    /// first; regeneration is free and lossless.
    finding: Option<String>,
    /// What the filter was meant to capture. Defaults to the description.
    filter_intent: Option<String>,
    /// Evidence the faster arm is still correct. logmon cannot know this, and
    /// "we asked and nobody said" is a different statement from silence.
    correctness_evidence: Option<String>,
    /// Break the ranked table down by "name" (default) or "group".
    group_by: Option<String>,
    /// Rows in the ranked table (default 15).
    top_n: Option<u32>,
    allow_mismatch: Option<bool>,
    allow_lossy: Option<bool>,
    allow_truncated: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
        let mut result = self
            .broker
            .call("status.get", serde_json::json!({}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        annotate_skew(&mut result);
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
                    "oneshot": p.oneshot,
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
                    "oneshot": p.oneshot,
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
                    "threshold": p.threshold.as_ref().map(|t| serde_json::json!({
                        "metric": t.metric,
                        "group": t.group,
                        "op": t.op,
                        "value": t.value,
                        "window_ms": t.window_ms,
                    })),
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
        // Built key-by-key rather than with `json!`, because `json!` renders a
        // `None` field as **`null`** — and on this one call `null` is not
        // "absent", it is "remove the threshold". Sending the whole struct would
        // clear the guard on every edit that did not mention it.
        //
        // The same shape used to be actively destructive here: `group_keys:
        // null` counted as a structural change, so an edit that only set a
        // description discarded the collector's live window. The daemon now
        // treats absent and null alike for every parameter except this one, but
        // building the object honestly is what keeps that from mattering again.
        let mut args = serde_json::Map::new();
        args.insert("name".into(), p.name.into());
        for (k, v) in [
            ("description", p.description),
            ("filter", p.filter),
            ("level", p.level),
            ("domain", p.domain),
        ] {
            if let Some(v) = v {
                args.insert(k.into(), v.into());
            }
        }
        if let Some(v) = p.group_keys {
            args.insert("group_keys".into(), serde_json::to_value(v).unwrap());
        }
        if let Some(v) = p.max_sample_bytes {
            args.insert("max_sample_bytes".into(), v.into());
        }
        // Present-and-null is the removal, so this arm must survive to the wire.
        if let Some(t) = p.threshold {
            args.insert("threshold".into(), t.unwrap_or(serde_json::Value::Null));
        }
        let result = self
            .broker
            .call("collectors.edit", serde_json::Value::Object(args))
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
                    "per_name": p.per_name,
                    "per_group": p.per_group,
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
        description = "Compare two runs and get what moved. Arms are \"<collector>\" (live \
                       window), \"<collector>@<label>\" (one recorded run), or \
                       \"<collector>@*\" (every recorded run merged). Prefer @* on both \
                       sides: with single runs there is no run-to-run spread, so nothing \
                       can separate a real change from scheduling noise, and every \
                       threshold comes back \"unknown\". Each row carries the threshold used \
                       to suppress it, and estimated percentile rows carry the error on the \
                       delta — at or above 100% the error bar is wider than the delta. \
                       REFUSES rather than guessing when the arms are not comparable, and \
                       names the flag that would permit it."
    )]
    async fn diff_collectors(
        &self,
        Parameters(p): Parameters<CollectorDiffParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "collectors.diff",
                serde_json::json!({
                    "a": p.a,
                    "b": p.b,
                    "group_by": p.group_by,
                    "top_n": p.top_n,
                    "allow_mismatch": p.allow_mismatch,
                    "allow_lossy": p.allow_lossy,
                    "allow_truncated": p.allow_truncated,
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

    #[rmcp::tool(
        description = "Write up a measurement: what moved, what to do next, and every caveat \
                       attached to the number it qualifies. Returns the document as text \
                       (markdown by default) plus a sidecar with the full percentile table -- \
                       write them yourself if you want them on disk. Pass `question` when you \
                       generate it and `finding` on a second call once you have read it; \
                       regeneration is free and lossless. `format: folded` gives collapsed \
                       stacks for a flame graph, one arm at a time, level tree only."
    )]
    async fn document_collectors(
        &self,
        Parameters(p): Parameters<CollectorDocumentParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "collectors.document",
                serde_json::json!({
                    "names": p.names,
                    "format": p.format,
                    "question": p.question,
                    "finding": p.finding,
                    "filter_intent": p.filter_intent,
                    "correctness_evidence": p.correctness_evidence,
                    "group_by": p.group_by,
                    "top_n": p.top_n,
                    "allow_mismatch": p.allow_mismatch,
                    "allow_lossy": p.allow_lossy,
                    "allow_truncated": p.allow_truncated,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        // The document itself, not a JSON envelope around it: a reader that has
        // to unwrap a string field before it can read the markdown will spend a
        // turn doing that, and the warnings are what it would skip.
        let mut out = result
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if let Some(ws) = result.get("warnings").and_then(|v| v.as_array()) {
            for w in ws.iter().filter_map(|w| w.as_str()) {
                out.push_str(&format!("\n\n> NOTE: {w}"));
            }
        }
        if let Some(name) = result.get("sidecar_name").and_then(|v| v.as_str()) {
            out.push_str(&format!(
                "\n\n> A sidecar `{name}` is available in this result's `sidecar_content`."
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
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
                    "start_seq": p.start_seq,
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

    #[rmcp::tool(
        description = "Record provenance for the bound domain — what was true of the project while these logs were produced. Logs without it are a dump; logs with it are evidence. Core keys: /Build/commit, /Build/profile, /Action. Also /Versions/<component>, /Env/host, /Data/seed (the one that turns \"fails 1 in 20\" into a reproduction). Send a value to set it, or a key alone to confirm what is already there. Returns one outcome per entry: created, updated, validated, unknown, or rejected."
    )]
    async fn update_domain_data(
        &self,
        Parameters(p): Parameters<UpdateDomainDataParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "domain_data.update",
                serde_json::json!({ "entries": p.entries }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Read the bound domain's provenance registry. Each key carries its value, when that value came into force, when it was last confirmed, and its age. A key with a stated lifetime also carries an expiry verdict; a key without one carries an age and NO verdict, deliberately — absent means unknown, not fresh. Also reports which recommended core keys are missing."
    )]
    async fn get_domain_data(
        &self,
        Parameters(p): Parameters<GetDomainDataParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut params = serde_json::Map::new();
        if let Some(prefix) = p.prefix {
            params.insert("prefix".into(), serde_json::Value::String(prefix));
        }
        if let Some(secs) = p.validated_before_secs {
            params.insert("validated_before_secs".into(), secs.into());
        }
        let result = self
            .broker
            .call("domain_data.get", serde_json::Value::Object(params))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(
        description = "Remove provenance keys by prefix, matched on segment boundaries (/Versions removes /Versions/*; /Ver removes nothing). Reports a count per pattern. THERE IS NO UNDO — and note that removing a key and setting it again resets when its value came into force, which turns a months-old confirmed fact into a fresh-looking one. To drop only a lifetime, send ttl: false to update_domain_data instead."
    )]
    async fn remove_domain_data(
        &self,
        Parameters(p): Parameters<RemoveDomainDataParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .broker
            .call(
                "domain_data.remove",
                serde_json::json!({ "patterns": p.patterns }),
            )
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

/// Tool names this build exposes, from the shared table.
fn local_tool_names() -> Vec<&'static str> {
    logmon_broker_protocol::mcp_tools::TOOLS
        .iter()
        .map(|(t, _)| *t)
        .collect()
}

/// Name, in the status payload, any tool the broker advertises that this build
/// cannot serve.
///
/// A **key**, not text appended after the JSON: `get_status` renders
/// `to_string_pretty`, so trailing prose would quietly turn a structured result
/// into an unparseable blob for anything that treats a tool result as JSON.
///
/// Silent in three cases, each for its own reason: a non-object payload (relay
/// it untouched rather than `unwrap` on `as_object_mut`), a broker too old to
/// send `broker_tools` (absent is unknown, not "you are missing everything"),
/// and an empty gap (a note in the everyday case is the false positive that
/// makes a reader stop believing the mechanism).
fn annotate_skew(result: &mut serde_json::Value) {
    let Some(obj) = result.as_object_mut() else {
        return;
    };
    // Never overwrite. `shim_note` is the obvious name for a *broker*-authored
    // message to stale shims, and clobbering it would silence the broker exactly
    // when it is ahead and has something to say — while the CHANGELOG promises
    // every key the broker sends arrives untouched.
    if obj.contains_key("shim_note") {
        return;
    }
    let Some(broker_tools) = obj.get("broker_tools").and_then(|v| v.as_array()) else {
        return;
    };
    let broker_tools: Vec<String> = broker_tools
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let version = obj
        .get("broker_version")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let Some(note) =
        logmon_broker_protocol::mcp_tools::skew_note(version, &broker_tools, &local_tool_names())
    else {
        return;
    };
    obj.insert("shim_note".into(), serde_json::Value::String(note));
}

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

    // -----------------------------------------------------------------------
    // The shared TOOLS table, held honest against what this shim actually built
    // -----------------------------------------------------------------------

    /// `TOOLS` is a hand-written mirror of the `#[rmcp::tool]` attributes, and a
    /// mirror that drifts is worse than none: a tool missing from the table gets
    /// reported to the user as unreachable when they are holding it, which is
    /// the false positive no reinstall can clear.
    ///
    /// This compares against the router rmcp *generates*, not against another
    /// list — the only comparison that cannot itself go stale. `tool_router()`
    /// is an associated fn (no broker, no daemon, no async) and is generated
    /// private, which is why this test lives inside `server.rs`.
    #[test]
    fn the_shared_tool_table_matches_the_router_this_shim_builds() {
        let mut from_table: Vec<String> = super::local_tool_names()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        from_table.sort();

        let mut from_router: Vec<String> = super::GelfMcpServer::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        from_router.sort();

        assert_eq!(
            from_table, from_router,
            "protocol::mcp_tools::TOOLS has drifted from the #[rmcp::tool] attributes"
        );
    }

    /// The *method* half of the table, checked against the call each tool body
    /// actually makes.
    ///
    /// The router test above compares tool names, and the broker-side test
    /// asserts each method exists — so swapping one real method for another
    /// (`collectors.snapshot` → `collectors.reset`) passes both while the table
    /// silently lies. Nothing reads the method half at runtime today, which is
    /// exactly why it would rot unnoticed until something did.
    ///
    /// Source-level, like the skill assertions above: parse this file, pair each
    /// `#[rmcp::tool]` fn with the first RPC literal in its body.
    #[test]
    fn every_tool_calls_the_method_the_shared_table_pairs_it_with() {
        const SRC: &str = include_str!("server.rs");

        // Two guards against the file scanning itself. The needle is assembled
        // rather than written, so this test's own source does not contain it;
        // and only the non-test half is scanned, so nothing below can add a
        // phantom tool. Without the first, the split found 43 "tools" in 42.
        let needle = concat!("#[rmcp::", "tool(");
        let src = SRC
            .split_once(concat!("#[cfg(", "test)]"))
            .map(|(before, _)| before)
            .unwrap_or(SRC);

        let mut found: Vec<(String, String)> = Vec::new();
        for chunk in src.split(needle).skip(1) {
            let Some(rest) = chunk.split_once("async fn ") else {
                continue;
            };
            let name: String = rest
                .1
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();

            // First `"group.verb"` literal in the body is the RPC it calls.
            let method = rest.1.split('"').find(|s| {
                s.contains('.')
                    && !s.contains(' ')
                    && s.split('.').count() == 2
                    && s.split('.').all(|p| {
                        !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                    })
            });
            if let Some(m) = method {
                found.push((name, m.to_string()));
            }
        }

        found.sort();
        let mut table: Vec<(String, String)> = logmon_broker_protocol::mcp_tools::TOOLS
            .iter()
            .map(|(t, m)| ((*t).to_string(), (*m).to_string()))
            .collect();
        table.sort();

        assert_eq!(
            found.len(),
            table.len(),
            "parsed {} tool bodies but the table has {} rows",
            found.len(),
            table.len()
        );
        assert_eq!(
            found, table,
            "a tool calls a different RPC than protocol::mcp_tools::TOOLS claims"
        );
    }

    // -----------------------------------------------------------------------
    // annotate_skew — every case but one needs synthetic input, because in a
    // single build the broker's list IS this build's list.
    // -----------------------------------------------------------------------

    fn status_with(broker_tools: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "daemon_uptime_secs": 1,
            "broker_version": "9.9.9",
            "broker_tools": broker_tools,
        })
    }

    #[test]
    fn a_broker_ahead_of_this_shim_names_exactly_the_missing_tools() {
        let mut v = status_with(&["get_status", "brand_new_tool", "another_new_one"]);
        super::annotate_skew(&mut v);
        let note = v["shim_note"].as_str().expect("a gap must be reported");
        assert!(note.contains("another_new_one") && note.contains("brand_new_tool"));
        assert!(
            note.contains("9.9.9"),
            "the broker version names the target"
        );
        assert!(
            note.contains("cargo install"),
            "a notice without the fix command is a complaint"
        );
    }

    #[test]
    fn a_matched_pair_is_left_silent() {
        // The everyday case. A note here is the false positive that teaches a
        // reader to ignore the mechanism.
        let names = super::local_tool_names();
        let mut v = status_with(&names);
        super::annotate_skew(&mut v);
        assert!(v.get("shim_note").is_none(), "no gap, so no note: {v}");
    }

    #[test]
    fn a_broker_too_old_to_advertise_says_nothing() {
        // Absent is unknown, not "you are missing everything".
        let mut v = serde_json::json!({ "daemon_uptime_secs": 1 });
        super::annotate_skew(&mut v);
        assert!(v.get("shim_note").is_none());
    }

    #[test]
    fn a_shim_ahead_of_its_broker_makes_no_reversed_claim() {
        let mut v = status_with(&["get_status"]);
        super::annotate_skew(&mut v);
        assert!(
            v.get("shim_note").is_none(),
            "holding more tools than the broker lists is not a gap in the shim"
        );
    }

    #[test]
    fn a_broker_authored_note_is_never_overwritten() {
        // `shim_note` is the obvious name for a broker-side message to stale
        // shims. Clobbering it would silence the broker exactly when it is ahead
        // and has something to say — and falsify the promise that every key the
        // broker sends arrives untouched.
        let mut v = status_with(&["get_status", "a_tool_this_shim_lacks"]);
        v.as_object_mut().unwrap().insert(
            "shim_note".into(),
            serde_json::Value::String("from the broker".into()),
        );
        super::annotate_skew(&mut v);
        assert_eq!(v["shim_note"], "from the broker");
    }

    #[test]
    fn a_non_object_payload_is_relayed_untouched() {
        let mut v = serde_json::json!("not an object");
        super::annotate_skew(&mut v);
        assert_eq!(v, serde_json::json!("not an object"));
    }

    #[test]
    fn the_daemons_own_keys_survive_annotation() {
        // The note is added to a passthrough. If annotation ever dropped or
        // rewrote a key, tier A's whole premise — that the daemon's response
        // reaches the reader verbatim — would be false for current shims.
        let before = status_with(&["get_status", "unreachable_tool"]);
        let mut after = before.clone();
        super::annotate_skew(&mut after);
        for (k, v) in before.as_object().unwrap() {
            assert_eq!(after.get(k), Some(v), "annotation altered `{k}`");
        }
        assert!(after.get("shim_note").is_some());
    }
}

#[cfg(test)]
mod param_drift_tests {
    use logmon_broker_protocol::mcp_tools::TOOLS;
    use std::collections::BTreeSet;

    /// The committed schema, generated from the protocol crate's request types
    /// by `cargo xtask gen-schema`.
    const PROTOCOL_SCHEMA: &str = include_str!("../../protocol/protocol-v1.schema.json");

    /// `collectors.edit` → `CollectorsEdit`. The generator names definitions
    /// after the Rust type, and the types are named after the methods, so the
    /// mapping is mechanical rather than a table someone has to maintain.
    fn definition_name(method: &str) -> String {
        method
            .split('.')
            .flat_map(|seg| seg.split('_'))
            .map(|word| {
                let mut c = word.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            })
            .collect()
    }

    /// Parameters deliberately not on both sides, each with the reason it is not
    /// drift. Anything not listed here is a bug, in one direction or the other.
    fn justified(tool: &str, param: &str) -> Option<&'static str> {
        match (tool, param) {
            // Client-side: the shim formats and writes the file itself, because
            // a service resolves a relative path against its own working
            // directory rather than the caller's.
            ("export_logs", "path") | ("export_logs", "format") => {
                Some("client-side file write, never sent to the daemon")
            }
            // The daemon reads it and refuses every non-default value
            // ("persistent domains are not yet supported"). Exposing it would
            // offer a knob whose only other setting is always an error.
            ("create_domain", "persist") => Some("daemon refuses true; not agent-facing yet"),
            _ => None,
        }
    }

    fn props(schema: &serde_json::Value) -> BTreeSet<String> {
        schema
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Every tool's parameters, against the daemon's own request type.
    ///
    /// This is the guard whose absence let eight parameters diverge unnoticed:
    /// the daemon honoured `oneshot`, `per_name`, `per_group`, `threshold`,
    /// `start_seq` and `description`, the skill documented several of them, and
    /// the shim had no field — so serde dropped the key and the call returned
    /// success. A permanent trigger, reported as the one-shot that was asked
    /// for.
    ///
    /// The existing source-scan tests could not catch it: one compares tool
    /// *names* against the router, the other pairs each tool with its *method*.
    /// Both were green throughout, and `skew_note` reported "45 of 45" the whole
    /// time, because it compares names too.
    #[test]
    fn no_tool_parameter_has_drifted_from_the_daemons_request_type() {
        let schema: serde_json::Value =
            serde_json::from_str(PROTOCOL_SCHEMA).expect("committed schema parses");
        let defs = schema
            .get("definitions")
            .and_then(|d| d.as_object())
            .expect("schema has definitions");

        let all = super::GelfMcpServer::tool_router().list_all();
        let mut problems: Vec<String> = Vec::new();

        for (tool, method) in TOOLS {
            let Some(route) = all.iter().find(|t| t.name == *tool) else {
                problems.push(format!("{tool}: in TOOLS but not in the router"));
                continue;
            };
            let shim = props(&serde_json::Value::Object((*route.input_schema).clone()));
            let def_name = definition_name(method);

            let Some(def) = defs.get(&def_name) else {
                // Not "this tool takes no parameters" — the generator's type
                // list is hand-maintained, so a missing definition means a live
                // tool whose parameters nothing describes.
                if !shim.is_empty() {
                    problems.push(format!(
                        "{tool} ({method}): no `{def_name}` in the committed schema, but the \
                         shim sends {shim:?}"
                    ));
                }
                continue;
            };
            let wire = props(def);

            for p in wire.difference(&shim) {
                if justified(tool, p).is_none() {
                    problems.push(format!(
                        "{tool}: the daemon accepts `{p}` and the shim cannot send it"
                    ));
                }
            }
            for p in shim.difference(&wire) {
                if justified(tool, p).is_none() {
                    problems.push(format!(
                        "{tool}: the shim sends `{p}` and `{def_name}` has no such field"
                    ));
                }
            }
        }

        assert!(
            problems.is_empty(),
            "shim/daemon parameter drift ({} problem(s)):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );
    }
}
