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
    /// Entries still to pass before this trigger can fire again — its own
    /// debounce window; `0` means armed and live. A trigger that looks stuck
    /// with a non-zero value here is debounced, not broken.
    #[serde(default)]
    pub post_remaining: u32,
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
    /// Mirrors `TriggerInfo::post_remaining`. This struct is deliberately
    /// separate from `TriggerInfo`, so a field added there does NOT appear
    /// here — the daemon emitted `post_remaining` on the edit response while
    /// this type lacked it, and serde silently dropped it for every typed SDK
    /// caller. `verify-schema` cannot catch that: it checks schema-vs-Rust,
    /// not daemon-JSON-vs-Rust.
    #[serde(default)]
    pub post_remaining: u32,
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
///
/// These statistics describe **every** stored span of this name that matched
/// the filter, not only the ones above `min_duration_ms`. The floor decides
/// which names appear, not which spans are counted — so a name can qualify on
/// its `max_ms` while its `avg_ms` sits far below the floor, and that gap is
/// the useful signal.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesSlowGroup {
    pub name: String,
    pub avg_ms: f64,
    #[serde(default)]
    pub p50_ms: f64,
    pub p95_ms: f64,
    /// The slowest span of this name. This is what the display floor tests.
    #[serde(default)]
    pub max_ms: f64,
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
    /// Grouped arm only: how many spans the aggregates were computed over.
    /// Stated because it is not `count` and not the number of rows — it is the
    /// whole matching population, which is the change that makes the grouped
    /// numbers trustworthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub population: Option<usize>,
    /// Grouped arm only: the `min_duration_ms` that decided which names appear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_floor_ms: Option<f64>,
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
// collectors.* / traces.profile
// =============================================================================

/// Why a field that normally carries a number is `null`.
///
/// A profile has several ways to be legitimately unable to answer — the
/// retention level is too low, a warm-up cut makes an unwindowed tier
/// meaningless, nothing nested so self time carries no information. Returning
/// `null` alone would leave the reader to guess which, and the guesses differ
/// in what they imply about the run. Every suppression names its field and its
/// reason so a consumer can distinguish "not measured" from "measured as zero".
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Suppressed {
    /// Dotted path of the suppressed field, e.g. `sampled.self_ms`.
    pub field: String,
    pub reason: String,
    /// What the caller could change to get a number instead. Absent when
    /// nothing would help.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

/// The window a profile covers. `wall_ms` runs from the later of arming and the
/// last zeroing, so a collector that was reset mid-run does not report the wall
/// time of data it no longer holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProfileWindow {
    pub armed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zeroed_at: Option<DateTime<Utc>>,
    pub read_at: Option<DateTime<Utc>>,
    pub wall_ms: f64,
}

/// Span loss during the window, and the collector's own view of unusable input.
///
/// `attribution` is `"domain"`, always: the transport counters are per-domain
/// and unfiltered, so most of what they count may be spans this collector's
/// filter would never have matched. That makes this sound as a trigger for
/// marking a result untrustworthy, and wrong as a statement about how many
/// matches went missing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProfileIngest {
    /// Per-span channel-full drops on the two OTLP trace transports.
    pub drops_in_window: u64,
    /// Whole request bodies refused with 429 / UNAVAILABLE. The span count
    /// behind these is unknowable — the bodies were never parsed.
    pub shed_batches: u64,
    /// Spans discarded at parse time for unusable identity.
    pub malformed_dropped: u64,
    /// Matched spans whose timestamps this collector could not use.
    pub malformed_timestamps: u64,
    /// Matched spans that ended before they started.
    pub negative_duration_spans: u64,
    pub attribution: String,
}

/// The always-exact tier. Never capped by the sample budget.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProfileExact {
    pub count: u64,
    pub total_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ms: Option<f64>,
    pub error_count: u64,
    /// Well-formed spans longer than the sketch's declared range: counted in
    /// full toward `total_ms`, clamped in the distribution.
    pub out_of_range_spans: u64,
}

/// Percentiles from the sketch — bounded relative error, unbounded population.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProfileEstimated {
    /// Which population this sketch covers: `collector`, `name`, or `group`.
    pub axis: String,
    /// The sketch's relative-accuracy guarantee, as a percentage.
    pub alpha_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p80_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_ms: Option<f64>,
}

/// Everything derived from retained per-span records. Exact over the records it
/// holds — which is the whole population only while `complete` is true.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProfileSampled {
    /// `false` once the sample budget stopped retention. The retained set is
    /// then a contiguous prefix, not a sample of the whole run.
    pub complete: bool,
    pub sample_count: u64,
    /// Duration minus the union of matched children clipped to this span —
    /// time spent in this span and nothing matched below it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_ms: Option<f64>,
    /// Matched spans whose parent is also matched. Zero means self time equals
    /// total time by construction, and `self_ms` is suppressed.
    pub nested_matches: u64,
    /// Child time that lay outside its parent's interval and was clipped away.
    /// Non-zero means clock skew or instrumentation error, not concurrency.
    pub overlapping_child_ms: f64,
    /// How many children needed clipping. Paired with the millisecond figure
    /// because one child off by a second and a thousand off by a millisecond
    /// want opposite remedies.
    pub overlapping_child_spans: u64,
    /// Elapsed time during which at least one matched span was in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_union_ms: Option<f64>,
    /// Total matched work divided by the wall time it occupied. 1.0 is fully
    /// serial; 4.0 means four matched spans in flight on average.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achieved_concurrency: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p80_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_ms: Option<f64>,
    /// Sample standard deviation (Bessel-corrected, `n-1`) of the durations the
    /// percentiles were computed over. Absent below two records, where it is
    /// undefined rather than zero.
    ///
    /// The n-1 form because at the sizes this matters for — a three-run A/B —
    /// the population form understates the spread by about 18%, and the whole
    /// question being asked is whether a difference exceeds the spread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stddev_ms: Option<f64>,
    /// Every retained duration, in **arrival order**, when the sample is
    /// complete and small enough to send (see `MAX_INLINE_DURATIONS`).
    ///
    /// Arrival order rather than sorted: a reader can sort these, but could not
    /// unsort them, and the ordering carries drift and warm-up trends that the
    /// percentiles have already thrown away.
    ///
    /// Absent when the sample was truncated — a prefix of a run presented as
    /// its durations would invite exactly the per-value reasoning that a biased
    /// subset cannot support — or when there are more than the cap. `suppressed`
    /// carries the reason in both cases. Only the top-level `sampled` block
    /// populates this: it is the population the headline figures describe, and
    /// per-group rows would multiply it by `top_n`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durations_ms: Option<Vec<f64>>,
}

/// One row of a breakdown. `key` is the span name, the group tuple, the trace
/// id, or the call path, depending on `grouped_by`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProfileGroup {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<ProfileExact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated: Option<ProfileEstimated>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled: Option<ProfileSampled>,
    /// Set on a call path whose walk stopped at a parent that is not retained,
    /// so the path is a suffix rather than a root-to-here chain.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub path_incomplete: bool,
}

/// One call path's self time, as a snapshot retains it (§9.8).
///
/// Recorded at snapshot time because the per-span records it is walked from are
/// not retained — a flame graph not taken then cannot be taken later. The list
/// is capped, and a snapshot says when it was cut short: a flame graph rendered
/// from a partial list is missing mass, and nothing about looking at one reveals
/// that.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PathRow {
    /// Matched ancestors, root first, **one frame per element**.
    ///
    /// A list rather than a joined string, because the joined form is
    /// ambiguous: a span named `parse > eval` is indistinguishable from two
    /// frames once ` > ` is the separator, and the flame-graph renderer that
    /// splits it back apart would silently produce a stack that never ran. The
    /// display form is derived; this is the form that survives.
    pub frames: Vec<String>,
    /// Time in this path and nothing matched below it.
    pub self_ms: f64,
    pub count: u64,
    /// The walk stopped at a parent that was not retained, so this is a suffix
    /// rather than a root-to-here chain. Rendered with a leading `[?]` frame.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
}

impl PathRow {
    /// The display form `a > b > c`, with `[?]` prefixed when the chain is a
    /// suffix. Matches the key `traces.profile` uses for a `path` row, so a path
    /// quoted from a profile can be found in a document.
    pub fn path(&self) -> String {
        let joined = self.frames.join(" > ");
        if self.incomplete {
            format!("[?] > {joined}")
        } else {
            joined
        }
    }
}

/// A rolling threshold as declared (§8).
///
/// Evaluated over a window **advanced by span arrival, never by a clock**, so an
/// idle collector costs nothing — and so a breached threshold on a finished run
/// neither fires again nor clears. That is the right behaviour for a load-time
/// guard and the wrong one for a liveness check, which is why every report
/// carries the note saying so.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ThresholdSpec {
    /// `count`, `total_ms`, `avg_ms`, `error_count` or `error_rate_pct`.
    ///
    /// **Percentiles are deliberately absent.** A rolling percentile needs a
    /// duration sketch per bucket, and this design's per-collector memory is
    /// bounded precisely so a collector cannot grow without limit. Use `avg_ms`
    /// for the guard and read the real percentiles with `collectors.get`.
    pub metric: String,
    /// Restrict to one group value. Requires the collector to declare
    /// `group_keys`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// `gt`, `gte`, `lt` or `lte`.
    pub op: String,
    pub value: f64,
    pub window_ms: u64,
}

/// A threshold and its current verdict, as `collectors.list` and
/// `collectors.get` report it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ThresholdInfo {
    pub metric: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub op: String,
    pub value: f64,
    /// The window as declared.
    pub window_ms: u64,
    /// The window as **evaluated**: the declared width rounded up to a whole
    /// number of ring buckets, so it is never narrower than what was asked for.
    /// Equal to `window_ms` when that is a multiple of the bucket count.
    #[serde(default)]
    pub effective_window_ms: u64,
    pub breached: bool,
    /// Clear-to-breached **transitions**, not evaluations: a threshold breached
    /// for a whole run has fired once, which is what "did this happen" means.
    pub fires: u64,
    /// The metric's value at the last evaluation. **Absent until one has
    /// happened — unknown, not zero**, because zero is a value `count` can
    /// legitimately hold and an `lt` threshold would read it as a breach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_value: Option<f64>,
    /// Whether any span has yet been evaluated against this threshold.
    pub evaluated: bool,
    /// Carried with every report, because it is the one thing about this
    /// mechanism that surprises people.
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsAdd {
    pub name: String,
    pub filter: String,
    /// `scalar`, `timing`, or `tree` (default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// Span attributes to split the numbers by. Values are read directly, so
    /// booleans and numbers work — unlike the filter path, which only sees
    /// strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_keys: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sample_bytes: Option<u64>,
    /// A rolling guard over this collector's matched spans (§8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<ThresholdSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsAddResult {
    pub name: String,
    pub filter: String,
    pub level: String,
    #[serde(default)]
    pub group_keys: Vec<String>,
    /// The domain the collector is pinned to. Rebinding the session later does
    /// not move it.
    pub domain: String,
    pub max_sample_bytes: u64,
    /// Filters that are legal but probably not what was meant — a mistyped
    /// selector, a status match that also admits errors, a no-op threshold.
    /// Never a refusal: each of these collects *something*.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorInfo {
    pub name: String,
    pub filter: String,
    pub level: String,
    #[serde(default)]
    pub group_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub domain: String,
    pub matched: u64,
    pub armed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zeroed_at: Option<DateTime<Utc>>,
    /// Why the live window is empty, when there is a reason: `daemon_restart`,
    /// `reset`, or `edit`. Absent means nothing has zeroed it — so a caller
    /// seeing `matched: 0` can tell "no traffic yet" from "the run you were
    /// measuring did not survive".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zeroed_by: Option<String>,
    /// Recorded runs held for this collector.
    #[serde(default)]
    pub snapshots: usize,
    /// `false` once the sample budget stopped retention.
    pub sample_complete: bool,
    /// The pinned domain no longer exists, so this collector can never match
    /// another span. The normal outcome of a restart for one armed on an
    /// ephemeral domain, since those are not re-created.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub orphaned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphan_note: Option<String>,
    /// The armed threshold and whether it is currently breached (§8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<ThresholdInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsListResult {
    pub collectors: Vec<CollectorInfo>,
    pub count: usize,
    /// Sample bytes reserved across every armed collector, daemon-wide.
    pub reserved_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsGet {
    pub name: String,
    /// Read a recorded snapshot by label instead of the live window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// `name`, `group`, `trace`, or `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_warmup_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<u64>,
}

/// `collectors.reset` / `collectors.remove`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsName {
    pub name: String,
}

/// What a reset threw away. Returned so a reset issued one call too early is
/// not silently unrecoverable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsDiscarded {
    pub matched: u64,
    pub total_ms: f64,
    pub error_count: u64,
    pub window_start: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsResetResult {
    pub name: String,
    pub discarded: CollectorsDiscarded,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsRemoveResult {
    pub removed: String,
    pub reserved_bytes: u64,
}

/// An edit (§7.1). `description` is free; **any other field discards the live
/// window** and re-runs every gate `collectors.add` runs. Recorded snapshots
/// are untouched and each keeps the definition it was taken under.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsEdit {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// `scalar`, `timing` or `tree`. Permitted in **both** directions: a `tree`
    /// collector heading for the sample cap mid-run can drop to `timing` for
    /// 2.5× the records, which is the only remedy left once the daemon-wide
    /// reservation is exhausted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_keys: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sample_bytes: Option<u64>,
    /// Re-pin to another domain. Only while the collector is zeroed, so a
    /// recorded window and the domain it was measured on cannot disagree. This
    /// is the remedy for a collector orphaned by a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Set or replace the rolling guard. Pass JSON `null` to remove one — which
    /// is why this is doubly optional: "leave it alone" and "take it away" are
    /// different requests and a single `Option` cannot express both.
    ///
    /// Like every other structural field, changing it **zeroes the live
    /// window**: the rolling ring is measurement state, and evaluating a new
    /// limit over a window accumulated under the old one would breach for
    /// reasons unrelated to load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<Option<ThresholdSpec>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsEditResult {
    pub name: String,
    pub filter: String,
    pub level: String,
    #[serde(default)]
    pub group_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub domain: String,
    pub max_sample_bytes: u64,
    /// Whether the live window was discarded. History never is.
    pub zeroed: bool,
    pub note: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsSnapshot {
    pub name: String,
    /// Unique per collector, `[A-Za-z0-9._-]`. Omitted → `snapshot-<n>`, and
    /// `n` never repeats over a collector's life even after eviction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// What this run was. Recorded with the data, so a number found weeks
    /// later still says what it measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Provenance logmon cannot infer — commit, build profile, config. Free
    /// form, and per-snapshot because two arms of a comparison differ in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    /// Zero the collector and start a fresh window. Default `true` — the
    /// point of a snapshot is usually to end one run and begin the next.
    #[serde(default)]
    pub reset: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_name: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_group: Option<bool>,
    /// Store the sample-derived figures. Computed now or never — the samples
    /// themselves are not retained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projections: Option<bool>,
}

/// One recorded run, as `collectors.history` lists it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotSummary {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    pub taken_at: Option<DateTime<Utc>>,
    pub window_start: Option<DateTime<Utc>>,
    pub wall_ms: f64,
    /// The definition **as of this snapshot**, never the live collector's.
    pub filter: String,
    pub level: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_keys: Vec<String>,
    pub exact: ProfileExact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated: Option<ProfileEstimated>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled: Option<ProfileSampled>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest: Option<ProfileIngest>,
    /// `false` if the sample budget truncated during this window. Travels with
    /// the record so a snapshot cannot launder a truncated run.
    pub sample_complete: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cardinality_capped: bool,
    /// Absent means durable. `false` means the run is in this response and in
    /// the daemon's memory but was not written, so it will not survive a
    /// restart — which matters to anyone collecting runs to compare later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durability_warning: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsHistory {
    pub name: String,
    /// Most recent N. Omitted → all retained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Also return the snapshots combined into one exact tier. Sample-derived
    /// figures do not merge and are omitted with a reason.
    #[serde(default)]
    pub merge: Option<bool>,
}

/// Spread across repeats of one configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RunToRunFloor {
    pub metric: String,
    pub runs: usize,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    /// Coefficient of variation, as a percentage. **Absent for a single run —
    /// unknown, not zero.** A floor of zero would license calling any
    /// difference significant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cv_pct: Option<f64>,
    /// What this figure does *not* cover. Carried with it because a bare
    /// spread invites being read as a total error bar.
    pub caveat: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsHistoryResult {
    pub collector: String,
    pub snapshots: Vec<SnapshotSummary>,
    pub count: usize,
    /// Snapshots dropped to the retention cap. Reported rather than inferred:
    /// a caller comparing "the first run" needs to know it is gone.
    pub evicted: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged: Option<ProfileExact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_estimated: Option<ProfileEstimated>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<RunToRunFloor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppressed: Vec<Suppressed>,
}

/// `collectors.diff` — compare two arms (§5.6).
///
/// An arm is `<collector>` for the live window, `<collector>@<label>` for one
/// recorded run, or `<collector>@*` for every recorded run merged. The merged
/// form is the only one that yields a run-to-run floor, which is what decides
/// whether a delta is a result or scheduling noise.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsDiff {
    pub a: String,
    pub b: String,
    /// `name` or `group`. Not `trace` or `path`: both are projected from
    /// per-span records, and a recorded run does not retain those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<u64>,
    /// Compare arms whose filters matched different populations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_mismatch: Option<bool>,
    /// Compare when one arm lost spans and the other did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_lossy: Option<bool>,
    /// Compare when both arms hit the sample budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_truncated: Option<bool>,
}

/// One side of a comparison, as the result describes it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DiffArm {
    /// How the caller named it.
    pub spec: String,
    pub collector: String,
    /// Which runs this figure came from, and how they were combined. **Always
    /// present**: a reader looking at a single number cannot otherwise tell one
    /// run from the mean of five, and the floor depends on which it is.
    pub aggregation: String,
    /// The definition **as of the measurement**, never the live collector's.
    pub filter: String,
    pub level: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Provenance logmon cannot infer — commit, build profile, configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    pub matched: u64,
    pub wall_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_start: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken_at: Option<DateTime<Utc>>,
    /// `detected`, `undetected`, or `unknown`.
    pub nesting: String,
    pub sample_complete: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cardinality_capped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest: Option<ProfileIngest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<RunToRunFloor>,
}

/// Something that differs between the arms, or that makes a number unsafe to
/// act on. Never silently absorbed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DiffMark {
    /// `level`, `filter`, `group_keys`, `ingest_loss`, `truncated`, `nesting`,
    /// `cardinality`, or `collector`.
    pub kind: String,
    pub detail: String,
    /// The flag the caller passed to permit this. Absent for conditions that
    /// only ever mark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overridden_by: Option<String>,
}

/// One metric's delta, with everything needed to decide whether to believe it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DiffRow {
    pub metric: String,
    /// `exact`, `estimated`, or `sampled` — never mixed within a row. The two
    /// can differ by 40 % under truncation, so a row taking one value from each
    /// would be uninterpretable.
    pub tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<f64>,
    /// Change from `a` to `b`, as a percentage of `a`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_pct: Option<f64>,
    /// **The threshold, in the metric's own units.** A delta at or below this is
    /// suppressed — and this is the number displayed beside it, because striking
    /// a delta through using a bound other than the one shown is how a reader
    /// ends up unable to check the tool's own reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_abs: Option<f64>,
    /// The same threshold as a percentage of the two values' mean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_pct: Option<f64>,
    /// `run-to-run`, `measurement-resolution`, both, or `unknown`.
    pub threshold_basis: String,
    /// §5.6's `α(a+b)/|a−b|`: the worst-case error on this delta as a
    /// percentage **of the delta**, on `estimated` rows only. At or above 100 %
    /// the error bar is wider than the delta it qualifies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_bound_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suppressed: bool,
    pub trustworthy: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// One breakdown key's rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DiffGroup {
    pub key: String,
    /// `both`, `a_only`, or `b_only`. A key on one arm only gets no delta: it
    /// may be missing because nothing matched it, or because a cardinality cap
    /// folded it into `__overflow__`, and those want opposite readings.
    pub presence: String,
    pub rows: Vec<DiffRow>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsDiffResult {
    pub a: DiffArm,
    pub b: DiffArm,
    /// The level both arms can answer at — `min(level)`.
    pub level: String,
    /// False when any mark makes a number unsafe to act on, **or** when either
    /// arm is a single run. A reader who reads nothing else should still not be
    /// misled.
    pub trustworthy: bool,
    pub rows: Vec<DiffRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grouped_by: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<DiffGroup>,
    /// Comparable keys **before** `top_n` truncation, so a reader can tell "the
    /// top 15 of 15" from "the top 15 of 200".
    #[serde(default)]
    pub groups_total: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<DiffMark>,
    /// How many group rows were dropped because a cardinality cap folded them
    /// into `__overflow__` on at least one arm. A count rather than a flag: one
    /// folded row and two hundred of them mean different things about whether
    /// the breakdown below is worth reading.
    #[serde(default)]
    pub overflow_rows_suppressed: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppressed: Vec<Suppressed>,
}

/// `collectors.document` — render a measurement for a reader (§9).
///
/// **The daemon returns bytes; the client writes them** (§9.9). The broker runs
/// as a service, so a relative path would resolve against its working directory
/// rather than the caller's — and `logs.export` already puts the write in the
/// client for the same reason. Regeneration is free and lossless, so nothing is
/// stored: the normal shape is to read the document, learn something, and
/// regenerate it with `finding` filled in.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsDocument {
    /// The arms to document. The **first is the baseline**; every other one is
    /// compared against it. Same syntax as `collectors.diff`.
    pub names: Vec<String>,
    /// `md` (default), `json`, or `folded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// What you were trying to find out. Carries most of the triage weight when
    /// this is read months later, which is why it is asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// What you concluded. Normally supplied on a **second** call, after the
    /// first read — which is why regeneration is free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding: Option<String>,
    /// What the filter was meant to capture. Defaults to the collector's
    /// description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_intent: Option<String>,
    /// Evidence that the faster arm is still correct. logmon cannot know this,
    /// and "we asked and nobody supplied it" is a different statement from
    /// silence — so the field is always present and defaults to `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correctness_evidence: Option<String>,
    /// `name` (default) or `group`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_mismatch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_lossy: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_truncated: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CollectorsDocumentResult {
    pub content: String,
    pub format: String,
    pub bytes: u64,
    /// The bulk companion (§9.7) — a percentile table plus layout identity,
    /// which is what decides whether two documents are comparable. Named in the
    /// document's own front-matter so the two can be reunited after being moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar_content: Option<String>,
    /// Things to know about the *document*, as distinct from the measurement it
    /// describes — a path list that was capped, detail moved to the sidecar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_keys: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_warmup_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<u64>,
}

/// The result of profiling a collector or an ad-hoc span query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProfileResult {
    /// Collector name, or absent for an ad-hoc `traces.profile` query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collector: Option<String>,
    /// Set when this is a recorded run rather than the live window. The label
    /// is how the run is referred to everywhere else, so a result without it
    /// would be unquotable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub filter: String,
    /// `scalar`, `timing`, or `tree`.
    pub level: String,
    pub matched: u64,
    /// `detected`, `undetected`, or `unknown` — the last when the retention
    /// level cannot answer the question at all.
    pub nesting: String,
    pub window: ProfileWindow,
    /// Absent when the pinned domain is gone, or was recreated under the
    /// collector, which makes a counter delta meaningless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest: Option<ProfileIngest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<ProfileExact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated: Option<ProfileEstimated>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled: Option<ProfileSampled>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grouped_by: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ProfileGroup>,
    /// Group keys **before** `top_n` truncation, so a reader can tell "the top
    /// 20 of 20" from "the top 20 of 900". Mirrors `CollectorsDiffResult`.
    #[serde(default)]
    pub groups_total: usize,
    /// How many retained spans `skip_warmup_ms` excluded from the sample tier.
    ///
    /// **Absent means the filter never ran** — not that it ran and excluded
    /// nothing. A reader that cannot tell those apart cannot tell "warm-up was
    /// negligible" from "warm-up was never cut", which are opposite facts about
    /// the same number. `sampled.sample_count` is the population that survived,
    /// so the two together give the whole picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_by_warmup: Option<u64>,
    /// True when a cardinality cap folded values into `__overflow__`, so at
    /// least one row aggregates an arbitrary, arrival-ordered member set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cardinality_capped: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppressed: Vec<Suppressed>,
    /// `traces.profile` only — the same admission warnings `collectors.add`
    /// returns, since an ad-hoc filter has no arm-time moment to report them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The armed threshold and its verdict, so one read answers both "what are
    /// the numbers" and "did it go over" (§8). Absent on a recorded run: a
    /// rolling window is live state and a snapshot does not preserve it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<ThresholdInfo>,
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

/// Span loss on the two OTLP trace transports — surfaced as a SIBLING of
/// [`ReceiverDropCounts`] on purpose, not folded into it.
///
/// `receiver_drops` documents itself as counting entries the broker lost
/// *silently* — the channel was full and the sender was never told. None of
/// these three counters describe that. A shed batch is a 429 / UNAVAILABLE
/// the caller saw and can retry; a malformed span was refused for cause, not
/// backpressure. Merging any of them into `receiver_drops` would change what
/// that field means. Keep them apart — do not "fix" this into a merge.
///
/// **`dropped` is not new information: it is exactly
/// `receiver_drops.otlp_http_traces + receiver_drops.otlp_grpc_traces`**, read
/// from the same two atomics. It is repeated here so the three trace-loss
/// figures can be read as one block, and it is stated here because **adding it
/// to those fields double-counts.** `shed_batches` and `malformed_dropped` are
/// the genuinely new numbers — nothing else in this payload reports them.
///
/// Scope: the calling session's bound domain, monotonic since that domain's
/// counters were created (daemon start for `default`, domain creation for
/// any other domain).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TraceIngestCounts {
    /// Per-span channel-full drops on the two OTLP trace transports.
    pub dropped: u64,
    /// Whole request bodies refused with 429 / UNAVAILABLE. The span count
    /// behind these is unknowable — the bodies were never parsed.
    pub shed_batches: u64,
    /// Spans discarded at parse time for unusable identity. Named
    /// `malformed_dropped` rather than `malformed`: `store.malformed_count`
    /// (log-side) already sits in this same payload, and the two must not
    /// read as one number.
    pub malformed_dropped: u64,
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
    /// Spans lost on the OTLP trace transports before any collector saw
    /// them — a sibling of `receiver_drops`, not a member (§ doc on
    /// [`TraceIngestCounts`]). Non-zero means every span-derived number
    /// elsewhere in this payload (and in `collectors.*`) is a lower bound.
    #[serde(default)]
    pub trace_ingest: TraceIngestCounts,
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
    /// Sessions currently bound to this domain (connected first, disconnected
    /// suffixed). DERIVED from the session registry — never caller-supplied —
    /// so with meaningful session names, `domains.list` answers "which
    /// conversation is using which domain" authoritatively. Populated by
    /// `domains.list`; empty elsewhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bound_sessions: Vec<String>,
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
/// entry), plus a warning when the bind REPLACED a different non-default
/// binding.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DomainsUseResult {
    #[serde(flatten)]
    pub domain: DomainInfo,
    /// Set when this bind switched the session from one NON-default domain to
    /// a different one. A session's normal lifecycle is `default → its domain`,
    /// then idempotent re-binds; a non-default→non-default switch is the
    /// signature of two clients sharing one session (e.g. two agent
    /// conversations behind one shared MCP server process), each rebinding the
    /// other's reads out from under it. The bind still succeeds — the warning
    /// rides the response so BOTH consumers of the shared stream see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebind_warning: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainsClear {}

/// `session.rename` — re-key the calling session to a meaningful name
/// (`<Project>-Main-<short8>` / `<Project>-tN-<branch>`, spec 2026-07-17),
/// preserving all state. Errors with "already connected" when the target name
/// is held by a LIVE session — the one-conversation-per-lane invariant.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionRename {
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionRenameResult {
    /// The session's new name (echoed; the connection re-keys itself on it).
    pub name: String,
    /// Set when a stale (disconnected) holder of this name was displaced.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub displaced_stale_holder: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainsClearResult {
    /// Number of log records disposed from the bound domain.
    pub logs_cleared: usize,
    /// Number of spans disposed from the bound domain.
    pub spans_cleared: usize,
}
