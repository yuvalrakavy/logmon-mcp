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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Level {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

/// Origin of a log entry within the pipeline. Mirrors `core::gelf::message::LogSource`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum LogSource {
    #[default]
    Filter,
    PreTrigger,
    PostTrigger,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    #[default]
    Unspecified,
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

/// Span status. Wire format mirrors `core::span::types::SpanStatus`:
/// `{"type":"unset"}`, `{"type":"ok"}`, `{"type":"error","message":"..."}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type", content = "message")]
pub enum SpanStatus {
    #[default]
    Unset,
    Ok,
    Error(String),
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
    /// Number of records that MATCHED the filter and were returned (capped by
    /// the requested `count`). This is the "matched" count in B2's vocabulary.
    pub count: usize,
    /// Number of buffered records EXAMINED to produce this result (B2). With
    /// `count == 0`, `scanned > 0` means "filter's fault, data is flowing",
    /// while `scanned == 0` means an empty buffer / dead pipeline.
    #[serde(default)]
    pub scanned: usize,
    /// Total records currently in the queried buffer.
    #[serde(default)]
    pub buffer_total: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_oldest_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_newest_seq: Option<u64>,
    /// B5: true when the query's lower bound (a `b>=` / `c>=` cursor) predates
    /// the oldest retained record — some in-window records were evicted, so this
    /// result is not the complete window.
    #[serde(default)]
    pub truncated: bool,
    /// Seqs between the requested window start and the oldest retained record
    /// (an upper bound on how many records rolled off), when `truncated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evicted_before_window: Option<u64>,
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
    #[serde(default)]
    pub scanned: usize,
    #[serde(default)]
    pub buffer_total: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_oldest_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_newest_seq: Option<u64>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evicted_before_window: Option<u64>,
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
    /// Number of traces MATCHED and returned (capped by `count`).
    pub count: usize,
    /// Spans examined to build these traces. `recent_traces` walks the whole
    /// span buffer, so for traces this equals `buffer_total` (B2): `count==0,
    /// scanned>0` means "filter's fault", `scanned==0` means empty span buffer.
    #[serde(default)]
    pub scanned: usize,
    /// Total spans currently in the span buffer.
    #[serde(default)]
    pub buffer_total: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_oldest_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_newest_seq: Option<u64>,
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
    #[serde(default)]
    pub gelf_udp: u64,
    #[serde(default)]
    pub gelf_tcp: u64,
    #[serde(default)]
    pub otlp_http_logs: u64,
    #[serde(default)]
    pub otlp_http_traces: u64,
    #[serde(default)]
    pub otlp_grpc_logs: u64,
    #[serde(default)]
    pub otlp_grpc_traces: u64,
}

/// Per-listener last-received wall-clock timestamps for the caller's bound
/// domain — the drill-down for "which port is (not) receiving." `None` = that
/// listener has received nothing. Mirrors [`ReceiverDropCounts`]. (Consumer #2.)
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ReceiverLiveness {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gelf_udp: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gelf_tcp: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_http_logs: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_http_traces: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_grpc_logs: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_grpc_traces: Option<DateTime<Utc>>,
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
    /// The domain the calling session is currently bound to (§7). Additive:
    /// an older daemon that omits it deserializes as `""`.
    #[serde(default)]
    pub current_domain: String,
    /// Echo of the session's active buffer-filter strings (§7) so a single
    /// status call answers "what is narrowing me". Additive.
    #[serde(default)]
    pub active_filters: Vec<String>,
    /// Per-listener last-received wall-clock for the caller's bound domain — the
    /// drill-down for "which port is (not) receiving." (Consumer #2.)
    #[serde(default)]
    pub receiver_liveness: ReceiverLiveness,
}

// =============================================================================
// domains.*
// =============================================================================

/// A single domain as reported by `domains.list` and `domains.create`.
///
/// A port value of `0` means that receiver is disabled for the domain. The
/// count/seq fields reflect the domain's live buffers; the `last_*_received_at` /
/// `idle_secs` / `stale` liveness fields answer "is this domain actually
/// receiving?" (`status.get.receiver_liveness` gives the per-listener drill-down).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainInfo {
    pub name: String,
    pub gelf_port: u16,
    pub otlp_grpc_port: u16,
    pub otlp_http_port: u16,
    /// `"config"` | `"persistent"` | `"ephemeral"`.
    pub source: String,
    pub log_buffer_size: usize,
    pub span_buffer_size: usize,
    pub log_count: usize,
    pub span_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_seq: Option<u64>,
    /// Wall-clock of the last log ingested into this domain (across any listener),
    /// or `None` if none ever arrived. A `None` here is the "nothing is shipping
    /// to this domain — did I misconfigure the port?" signal. (Consumer #2.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_log_received_at: Option<DateTime<Utc>>,
    /// Wall-clock of the last span ingested into this domain, or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_span_received_at: Option<DateTime<Utc>>,
    /// Seconds since the most recent log OR span ingest; `None` if neither ever
    /// arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_secs: Option<u64>,
    /// `true` when the domain HAS received before but has been idle longer than
    /// the broker's `stale_after_secs` (config, default 60s). A never-received
    /// domain is `false` — distinguish it via `last_*_received_at == None`.
    #[serde(default)]
    pub stale: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainsCreate {
    pub name: String,
    /// GELF UDP+TCP port. Omitted → auto-allocate; `0` → disable GELF for this
    /// domain; otherwise bind exactly this port (error if held by another domain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gelf_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_grpc_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_http_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_buffer_size: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_buffer_size: Option<usize>,
    /// `false` (default) → ephemeral (gone on restart). `true` → durable
    /// (re-created at boot). Durable domains are a later stage; Wave 2 accepts
    /// only ephemeral creation.
    #[serde(default)]
    pub persist: bool,
}

/// `domains.create` returns the created (or already-existing) domain, same
/// shape as a `domains.list` entry.
pub type DomainsCreateResult = DomainInfo;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainsDelete {
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainsDeleteResult {
    pub name: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainsList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainsListResult {
    pub domains: Vec<DomainInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainsUse {
    pub name: String,
}

/// `domains.use` returns the now-bound domain (same shape as a `domains.list`
/// entry).
pub type DomainsUseResult = DomainInfo;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainsClear {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainsClearResult {
    /// Number of log records disposed from the bound domain.
    pub logs_cleared: usize,
    /// Number of spans disposed from the bound domain.
    pub spans_cleared: usize,
}
