//! Typed parameter and result structs for every RPC method exposed by the
//! daemon.
//!
//! These types mirror the on-the-wire JSON shapes produced/consumed by
//! `crates/core/src/daemon/rpc_handler.rs`. They are additive: the daemon
//! still reads/writes `serde_json::Value` internally. The SDK (Task 13) and
//! the JSON Schema exporter (Task 8) consume these types.
//!
//! Naming convention: the param struct for `<group>.<verb>` is `<Group><Verb>`,
//! and the result struct is `<Group><Verb>Result`.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

// =============================================================================
// Shared payload types
// =============================================================================

/// Log severity level. Wire format is the variant name as written
/// (e.g. `"Info"`, `"Error"`), matching `core::gelf::message::Level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Default for Level {
    fn default() -> Self {
        Level::Info
    }
}

/// Origin of a log entry within the pipeline. Mirrors `core::gelf::message::LogSource`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum LogSource {
    Filter,
    PreTrigger,
    PostTrigger,
}

impl Default for LogSource {
    fn default() -> Self {
        LogSource::Filter
    }
}

/// A single log line as the daemon emits it. Mirrors `core::gelf::message::LogEntry`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogEntry {
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    pub level: Level,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_message: Option<String>,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub additional_fields: HashMap<String, Value>,
    /// Trace id rendered as 32-char lowercase hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Span id rendered as 16-char lowercase hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    #[serde(default)]
    pub matched_filters: Vec<String>,
    #[serde(default)]
    pub source: LogSource,
}

/// Span kind. Wire format is snake_case, matching `core::span::types::SpanKind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Unspecified,
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

impl Default for SpanKind {
    fn default() -> Self {
        SpanKind::Unspecified
    }
}

/// Span status. Wire format mirrors `core::span::types::SpanStatus`:
/// `{"type":"unset"}`, `{"type":"ok"}`, `{"type":"error","message":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type", content = "message")]
pub enum SpanStatus {
    Unset,
    Ok,
    Error(String),
}

impl Default for SpanStatus {
    fn default() -> Self {
        SpanStatus::Unset
    }
}

/// A timestamped event attached to a span. Mirrors `core::span::types::SpanEvent`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SpanEvent {
    pub name: String,
    pub timestamp: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub attributes: HashMap<String, Value>,
}

/// A single span. Mirrors `core::span::types::SpanEntry`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SpanEntry {
    pub seq: u64,
    /// Trace id rendered as 32-char lowercase hex.
    pub trace_id: String,
    /// Span id rendered as 16-char lowercase hex.
    pub span_id: String,
    /// Parent span id rendered as 16-char lowercase hex (none for root spans).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub duration_ms: f64,
    pub name: String,
    pub kind: SpanKind,
    pub service_name: String,
    pub status: SpanStatus,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub attributes: HashMap<String, Value>,
    #[serde(default)]
    pub events: Vec<SpanEvent>,
}

/// A high-level summary of a single trace. Mirrors `core::span::types::TraceSummary`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TraceSummary {
    /// Trace id rendered as 32-char lowercase hex.
    pub trace_id: String,
    pub root_span_name: String,
    pub service_name: String,
    pub start_time: DateTime<Utc>,
    pub total_duration_ms: f64,
    pub span_count: u32,
    pub has_errors: bool,
    pub linked_log_count: u32,
}

/// One entry in a `filters.list` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FilterInfo {
    pub id: u32,
    pub filter: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One entry in a `triggers.list` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggerInfo {
    pub id: u32,
    pub filter: String,
    pub pre_window: u32,
    pub post_window: u32,
    pub notify_context: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub match_count: u64,
    /// When `true`, the trigger auto-removes after its first match. Always
    /// serialized (no `skip_serializing_if`) so clients see the explicit value.
    #[serde(default)]
    pub oneshot: bool,
}

/// One entry in a `session.list` or `status.get` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub connected: bool,
    pub trigger_count: usize,
    pub filter_count: usize,
    pub queue_size: usize,
    pub last_seen_secs_ago: u64,
    /// Caller-supplied identity blob from the most recent `session.start`.
    /// Omitted from the wire when never set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<Value>,
}

/// Pipeline buffer / receive statistics. Embedded in `status.get`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StoreStats {
    pub total_received: u64,
    pub total_stored: u64,
    pub malformed_count: u64,
    pub current_size: usize,
}

/// One entry in a `bookmarks.list` response.
///
/// As of the cursor design (2026-05-01), bookmark position is a `u64` seq
/// rather than a wall-clock timestamp. `created_at` is retained for human
/// display only; never use it for filter semantics.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarkInfo {
    /// Qualified form (`"session/bookmark"`). The bare name is recoverable
    /// by splitting on the last `/`.
    pub qualified_name: String,
    /// Position the bookmark anchors. Filter DSL `b>=name` matches records
    /// with `entry.seq > seq`; `b<=name` matches `entry.seq < seq`.
    pub seq: u64,
    /// Wall-clock creation time, for display only.
    pub created_at: DateTime<Utc>,
    /// Optional caller-supplied note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// =============================================================================
// logs.*
// =============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsRecent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsRecentResult {
    pub logs: Vec<LogEntry>,
    pub count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_advanced_to: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsContext {
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsContextResult {
    pub logs: Vec<LogEntry>,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsExport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsExportResult {
    pub logs: Vec<LogEntry>,
    pub count: usize,
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_advanced_to: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsClear {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsClearResult {
    pub cleared: usize,
}

// =============================================================================
// filters.*
// =============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FiltersList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FiltersListResult {
    pub filters: Vec<FilterInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FiltersAdd {
    pub filter: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FiltersAddResult {
    pub id: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FiltersEdit {
    pub id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FiltersEditResult {
    pub id: u32,
    pub filter: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FiltersRemove {
    pub id: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FiltersRemoveResult {
    pub removed: u32,
}

// =============================================================================
// triggers.*
// =============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersListResult {
    pub triggers: Vec<TriggerInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersAdd {
    pub filter: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_context: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// When `true`, the trigger auto-removes after its first match. Defaults
    /// to `false`; old clients without this field continue to deserialize.
    #[serde(default)]
    pub oneshot: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersAddResult {
    pub id: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersEdit {
    pub id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_context: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oneshot: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersEditResult {
    pub id: u32,
    pub filter: String,
    pub pre_window: u32,
    pub post_window: u32,
    pub notify_context: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub match_count: u64,
    #[serde(default)]
    pub oneshot: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersRemove {
    pub id: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersRemoveResult {
    pub removed: u32,
}

// =============================================================================
// traces.* / spans.*
// =============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesRecent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesRecentResult {
    pub traces: Vec<TraceSummary>,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesGet {
    pub trace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_logs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesGetResult {
    pub trace_id: String,
    pub spans: Vec<SpanEntry>,
    pub logs: Vec<LogEntry>,
    pub span_count: usize,
    pub log_count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesSummary {
    pub trace_id: String,
}

/// One row in `TracesSummaryResult.breakdown`: the contribution of one direct
/// child of the root span to the trace's wall-clock time.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TraceSummaryBreakdownEntry {
    pub name: String,
    pub self_time_ms: f64,
    pub total_time_ms: f64,
    pub percentage: f64,
    pub is_error: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesSummaryResult {
    pub root_span: String,
    pub total_duration_ms: f64,
    pub breakdown: Vec<TraceSummaryBreakdownEntry>,
    pub other_ms: f64,
    pub span_count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesSlow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_duration_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// When set to `"name"`, results are aggregated into `groups`; otherwise
    /// individual spans are returned in `spans`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
}

/// One row in a grouped `TracesSlowResult` (when `group_by == "name"`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesSlowGroup {
    pub name: String,
    pub avg_ms: f64,
    pub p95_ms: f64,
    pub count: usize,
}

/// `traces.slow` returns either an ungrouped list of spans or a grouped
/// aggregation, depending on the `group_by` parameter. Both shapes are
/// expressed as optional fields on the same result struct.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesSlowResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spans: Option<Vec<SpanEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grouped_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<TracesSlowGroup>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesLogs {
    pub trace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesLogsResult {
    pub logs: Vec<LogEntry>,
    pub count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_advanced_to: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SpansContext {
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SpansContextResult {
    pub spans: Vec<SpanEntry>,
    pub count: usize,
}

// =============================================================================
// bookmarks.*
// =============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksAdd {
    pub name: String,
    /// Caller-chosen anchor seq. Defaults to the daemon's current seq counter
    /// (i.e., "right now") when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_seq: Option<u64>,
    /// Optional caller-supplied note retained for human display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksAddResult {
    pub qualified_name: String,
    /// Anchor seq the bookmark was created at.
    pub seq: u64,
    pub replaced: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksList {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksListResult {
    pub bookmarks: Vec<BookmarkInfo>,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksRemove {
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksRemoveResult {
    pub removed: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksClear {
    /// Defaults to the calling session when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksClearResult {
    pub removed_count: usize,
    pub session: String,
}

// =============================================================================
// session.* / status.*
// =============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionListResult {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionDrop {
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionDropResult {
    pub dropped: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StatusGet {}

/// Per-source receiver drop counts. Each field is the cumulative count of
/// entries dropped at that receiver call site since broker start, due to
/// the upstream channel being full (i.e. the consumer couldn't keep up).
/// Healthy operation keeps all fields at 0.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReceiverDropCounts {
    #[serde(default)] pub gelf_udp: u64,
    #[serde(default)] pub gelf_tcp: u64,
    #[serde(default)] pub otlp_http_logs: u64,
    #[serde(default)] pub otlp_http_traces: u64,
    #[serde(default)] pub otlp_grpc_logs: u64,
    #[serde(default)] pub otlp_grpc_traces: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StatusGetResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    pub daemon_uptime_secs: u64,
    pub receivers: Vec<String>,
    pub store: StoreStats,
    #[serde(default)]
    pub receiver_drops: ReceiverDropCounts,
}
