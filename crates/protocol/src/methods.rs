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

/// One entry in a `sessions.list` or `status.get` response.
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
#[serde(deny_unknown_fields)]
pub struct LogsRecent {
    /// Number of log entries to return (default: 50)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// Optional DSL filter expression
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Filter logs by trace ID (32-char hex)
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

// ---------------------------------------------------------------------------
// logs.fields — what dimensions exist in this buffer
// ---------------------------------------------------------------------------

/// What the observed values of a field look like.
///
/// Load-bearing rather than decorative: it is what tells a caller which fields
/// a numeric aggregation could ever sum, without sampling records to find out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ValueKind {
    String,
    Integer,
    Float,
    Bool,
    /// Integers and floats seen under one name. Still summable — an emitter
    /// writing `100` and `100.5` for one quantity is ordinary, and calling that
    /// `mixed` would warn against the one case where the warning is wrong.
    Number,
    /// Genuinely disagreeing kinds under one name (a string and a number, say).
    /// Not an error — emitters do this — but a caller must not assume it can be
    /// summed.
    Mixed,
    /// Null, array or object: present, but neither a dimension nor a number.
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TopValue {
    /// Rendered exactly as the filter matcher renders it, so this string can be
    /// pasted into a filter and will match.
    pub value: String,
    pub count: usize,
}

/// Where a field's value lives on a record — and therefore which filter
/// selector reaches it.
///
/// **A name alone does not identify a field.** GELF strips the `_` prefix, so a
/// payload sending both `file` and `_file` produces a top-level `LogEntry.file`
/// AND an `additional_fields["file"]`. Those are two different values reached by
/// two different selectors (`fi` versus `file`), and merging them into one row
/// double-counts coverage and hides the built-in's true (often zero) presence —
/// which is the one fact this whole read exists to surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldSource {
    /// A dedicated slot on the record: level, message, host, facility, file,
    /// line. Reached by a short DSL selector (`fi`, `ln`, …).
    Builtin,
    /// An underscore-prefixed GELF extra, stripped of its prefix. Reached by its
    /// own name.
    Additional,
    /// Lifted out of `additional_fields` by the parser (`trace_id`, `span_id`).
    /// Real, but **no log filter selector reaches it** — see `selector`.
    Promoted,
}

/// One field's row in the map.
///
/// Identified by `(source, field)`, not by `field` alone: two rows may share a
/// name and differ in source. Read `selector` to know what to actually type.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FieldStats {
    /// The field's name as it appears on the record.
    pub field: String,
    pub source: FieldSource,
    /// **The DSL spelling that matches this row**, ready to paste into a
    /// filter — `fi` for the built-in `file`, `file` for the additional field of
    /// the same name.
    ///
    /// `null` means no log filter selector reaches this field at all. That is
    /// the case for `trace_id` and `span_id`: the parser removes them from
    /// `additional_fields`, so `trace_id=…` in a filter would match nothing and
    /// report no error. Use the `trace_id` PARAMETER on `logs.recent` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// Records carrying it, out of `matched`.
    pub present: usize,
    /// Share of `matched` carrying it.
    pub coverage_pct: f64,
    /// Distinct values — **`null` once the cap was exceeded**, never a partial
    /// count presented as a total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinct: Option<usize>,
    pub top_values: Vec<TopValue>,
    pub kind: ValueKind,
}

/// `logs.fields` — the map an agent needs before it can name an axis.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogsFields {
    /// DSL filter describing the population to describe. Defaults to everything.
    ///
    /// Cursor qualifiers (`c>=`) are **rejected**: a cursor advances on read, so
    /// a second identical call would describe a different population.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Most-frequent values reported per field (default 3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_values: Option<u64>,
    /// Omit fields present on fewer than this share of matched records.
    /// `0` (the default) reports everything, **including fields at 0%** — an
    /// absent built-in is a fact worth seeing, not a row worth hiding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_coverage_pct: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsFieldsResult {
    /// Coverage descending, then field name — stable across identical calls.
    pub fields: Vec<FieldStats>,
    /// Records that passed the filter.
    pub matched: usize,
    /// Records EXAMINED — the whole ring, never a `count`-limited prefix. A
    /// description of the buffer that silently described its newest slice would
    /// be the exact sampling bias this family of reads exists to remove.
    pub scanned: usize,
    /// Total records currently in the queried buffer.
    pub buffer_total: usize,
    /// The ring's real bounds.
    ///
    /// Present because `truncated` can only speak about a WINDOW: it needs a
    /// resolved lower bound, so a call with no `b>=` reports `false` however
    /// much rolled off. These two, with `lost_below`, are how a caller sees a
    /// wrapped buffer regardless of the filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_oldest_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_newest_seq: Option<u64>,
    /// The lowest seq this buffer can still speak for; `0` means nothing has
    /// ever been evicted.
    #[serde(default)]
    pub lost_below: u64,
    /// True when the query's lower bound predates the oldest retained record,
    /// so this describes an incomplete window. **Requires a lower bound to be
    /// meaningful** — see `buffer_oldest_seq`.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evicted_before_window: Option<u64>,
    /// True when the field-NAME cap was reached and some names are missing from
    /// `fields`. A truncated map presented as the whole one would be the same
    /// silent-undercount this read exists to prevent.
    #[serde(default)]
    pub names_capped: bool,
}

/// `logs.profile` — how records distribute along one named axis.
///
/// The sibling of `logs.fields`: that one says which dimensions exist, this one
/// says how the population spreads along the one you pick. The axis vocabulary
/// mirrors `traces.profile` — a closed enum names the KIND of axis, an open
/// `group_keys` list names emitter fields — because that is the only spelling
/// that can reach every row `logs.fields` reports. `trace_id` and `span_id`
/// carry `selector: null`, so a filter-selector vocabulary could not name them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogsProfile {
    /// DSL filter describing the population to profile. Defaults to everything.
    ///
    /// Cursor qualifiers (`c>=`) are **rejected**: a cursor advances on read, so
    /// a second identical call would profile a different population.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// The axis. `field` takes its names from `group_keys`; everything else is
    /// a built-in or promoted field. `none`, `""` or omitted returns the
    /// ungrouped figures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("enum" = [
        "none", "", "level", "message", "host", "facility", "file", "line",
        "trace_id", "span_id", "field", null
    ]))]
    pub group_by: Option<String>,
    /// Emitter field names, at most 8. **Required non-empty when `group_by` is
    /// `field`, and an error, non-empty, on any other axis** — a caller who
    /// passes field names believes they are grouping by fields, so answering a
    /// different question silently is worse than one refused round trip.
    ///
    /// An explicit `[]` is NOT "present": it means the same as absent here.
    /// Elsewhere the two differ (`collectors.edit` treats `[]` as a structural
    /// clear), but this method persists nothing for an empty list to clear, and
    /// a generated client that always serialises `[]` would otherwise have its
    /// most ordinary call refused.
    ///
    /// More than one name forms a tuple, joined with ` / `.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_keys: Option<Vec<String>>,
    /// **Real** rows returned (default 20). The reserved buckets are additional
    /// — see `LogsProfileResult::groups`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<u64>,
}

/// Records per level. A fixed struct, never a map.
///
/// `Level` has five variants and no more (`gelf/message.rs`), so every field is
/// always emitted: a reader never has to tell "key absent" from "count zero",
/// which carries no information here and is a reliable source of downstream
/// `unwrap_or(0)`. It also lets the renderer lay out fixed columns in severity
/// order, and lets the schema state the whole domain instead of
/// `additionalProperties`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LevelCounts {
    pub trace: usize,
    pub debug: usize,
    pub info: usize,
    pub warn: usize,
    pub error: usize,
}

impl LevelCounts {
    pub fn total(&self) -> usize {
        self.trace + self.debug + self.info + self.warn + self.error
    }
}

/// One row of a `logs.profile` breakdown.
///
/// **Each end of the row is one whole record** — a seq, its timestamp, and the
/// message that was there. That is why `first_time` is the timestamp OF the
/// record at `first_seq` rather than `min(timestamp)`: `timestamp` is
/// emitter-supplied while `seq` is assigned at receipt, so reordering, several
/// senders or clock skew make those different records, and a row that mixed
/// them would describe two records at once.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogGroup {
    /// The value; `__absent__` or `__overflow__` for the reserved buckets;
    /// tuple members joined with ` / `.
    pub key: String,
    pub count: usize,
    pub levels: LevelCounts,
    pub first_seq: u64,
    pub first_time: DateTime<Utc>,
    /// The message at `first_seq`, **verbatim** — never synthesised. For a
    /// recurring failure the first occurrence usually carries the root cause
    /// and the later ones are cascade or retry.
    pub first_exemplar: String,
    pub last_seq: u64,
    pub last_time: DateTime<Utc>,
    /// The message at `last_seq`, verbatim. Read beside `first_exemplar` it
    /// answers a question neither alone can: did this group change shape over
    /// the window?
    pub last_exemplar: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsProfileResult {
    /// At most `top_n` **real** rows, plus up to two reserved rows
    /// (`__absent__`, `__overflow__`), which sort last and do not consume the
    /// budget — so asking for 20 rows buys 20 real values. `groups.len()` may
    /// therefore be as large as `top_n + 2`.
    ///
    /// **The rows sum to `matched` only when the real-key count is at most
    /// `top_n`.** `groups_total` is what makes the shortfall visible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<LogGroup>,
    /// The axis actually used. `null` when none was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grouped_by: Option<String>,
    /// Echoed so a reply is self-describing. `null` when none were given, or
    /// when an empty array normalised to none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_keys: Option<Vec<String>>,
    /// Distinct keys **before** truncation, `__absent__` and `__overflow__`
    /// included — the same convention as `ProfileResult::groups_total`, not
    /// `CollectorsDiffResult`'s.
    ///
    /// `null`, never `0`, when no grouping was performed: `0` would read as
    /// "this query touched nothing" rather than "we did not look".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups_total: Option<usize>,
    /// True when a cardinality cap folded keys into `__overflow__`, so at least
    /// one row aggregates an arbitrary, arrival-ordered member set.
    #[serde(default)]
    pub cardinality_capped: bool,
    /// Across the whole matched set, not just the returned rows.
    pub levels: LevelCounts,
    /// Records that passed the filter.
    pub matched: usize,
    /// Records EXAMINED — the whole ring, never a `count`-limited prefix.
    pub scanned: usize,
    /// Total records currently in the queried buffer.
    pub buffer_total: usize,
    /// Bounds of the matched set. `null` when nothing matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_time: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_time: Option<DateTime<Utc>>,
    /// The ring's real bounds — the only way a caller sees a wrapped buffer
    /// when the filter carries no lower bound, in which case `truncated` is
    /// structurally false however much rolled off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_oldest_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_newest_seq: Option<u64>,
    /// The lowest seq this buffer can still speak for; `0` means nothing has
    /// ever been evicted.
    #[serde(default)]
    pub lost_below: u64,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evicted_before_window: Option<u64>,
    /// Where an axis absent from every matched record is announced, and where a
    /// structured value that cannot be a dimension is accounted for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppressed: Vec<Suppressed>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogsContext {
    /// Sequence number of the anchor entry
    pub seq: u64,
    /// Number of entries before the anchor (default: 10)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    /// Number of entries after the anchor (default: 10)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsContextResult {
    pub logs: Vec<LogEntry>,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogsExport {
    /// Maximum number of entries to export
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// Optional DSL filter expression
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Lowest seq to export, **inclusive**. Composes with `filter`: a range and
    /// a bookmark bound together resolve to the tighter of the two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_seq: Option<u64>,
    /// Highest seq to export, **inclusive**.
    ///
    /// Inclusive on both ends because the alternative has an off-by-one at the
    /// centre of every window: a range built as "the anchor, plus 0 before and
    /// 0 after" must contain the anchor, and a strict bound would exclude the
    /// one entry the caller was pointing at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_seq: Option<u64>,
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
    /// `count` stopped this short of everything that matched the range.
    ///
    /// Stated rather than left to be inferred, because it **cannot** be
    /// inferred: comparing what was asked for against what came back cannot
    /// tell a capped range apart from one where exactly that many existed. A
    /// reader deciding whether a window is complete needs that difference, and
    /// `truncated` will not supply it — that reports *eviction*, which is a
    /// different kind of missing.
    #[serde(default)]
    pub capped: bool,
    /// How much of this window the daemon can vouch for.
    ///
    /// Defaults to `cannot_verify`, which is the whole point of the default: a
    /// reply from a broker too old to send the field must not be read as a
    /// clean bill of health.
    #[serde(default)]
    pub verdict: EvidenceVerdict,
    /// The stretches of the window over which a session filter was narrowing
    /// what the daemon stored, and the filters doing it. Empty unless `verdict`
    /// is `filtered`.
    ///
    /// Always emitted, including empty — a reader that sees `filtered` comes
    /// here next, and "the field is absent" must not be one more state it has
    /// to tell apart from "nothing narrowed anything".
    #[serde(default)]
    pub narrowed_by: Vec<NarrowedRange>,
}

/// How much of an exported window the daemon can vouch for.
///
/// The reason this exists: storage is conditional, so "nothing appeared before
/// the error" is ambiguous between *absence of cause* and *absence of
/// recording*. Only a record of the storage policy over time can separate them,
/// and without one a window shaped by another session's filter reads as
/// complete.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceVerdict {
    /// The window lies wholly inside one unfiltered epoch, nothing was evicted,
    /// and nothing was capped. Everything that reached the daemon over these
    /// seqs **was stored**.
    ///
    /// It does *not* say everything stored is in this reply: your own `filter`
    /// still selects, and `count` still limits. The verdict is about the
    /// daemon's storage policy over the range — which is the only thing a later
    /// read cannot recover — not about what this particular call returned.
    Complete,
    /// The ring dropped part of the window.
    ///
    /// On an export, `evicted_before_window` bounds how much — the window's
    /// lower end and the store's floor are independent numbers there, so their
    /// difference means something. On a **capture** there is no such bound and
    /// none is reported: the window opens at the first record that SURVIVED, so
    /// the two coincide, and the document states the floor and the caller's
    /// shortfall as the separate facts they are rather than passing one off as
    /// a count of the other.
    Evicted,
    /// A session filter was narrowing the store over part of the window. See
    /// `narrowed_by` for which filters, and over which seqs.
    Filtered,
    /// No claim can be made: the store is empty, or the window predates what
    /// the daemon still records about its own storage policy (including a
    /// window carried over from a previous daemon run), or the read was capped
    /// short of what matched.
    ///
    /// The default, deliberately — see [`LogsExportResult::verdict`].
    #[default]
    CannotVerify,
}

/// One stretch of an exported window over which a session filter was narrowing
/// what the daemon stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NarrowedRange {
    /// Clamped to the window that was asked for, so these are the caller's own
    /// seqs rather than wherever the epoch happened to begin.
    pub from_seq: u64,
    pub to_seq: u64,
    /// The filter strings in force over this stretch — the union across every
    /// session bound to the domain, since any one of them narrowing the store
    /// narrows it for everybody.
    pub filters: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogsClear {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsClearResult {
    pub cleared: usize,
}

// =============================================================================
// filters.*
// =============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FiltersList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FiltersListResult {
    pub filters: Vec<FilterInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FiltersAdd {
    /// DSL filter expression
    pub filter: String,
    /// Human-readable description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FiltersAddResult {
    pub id: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FiltersEdit {
    /// Filter ID to edit
    pub id: u32,
    /// New DSL filter expression
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// New description
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
#[serde(deny_unknown_fields)]
pub struct FiltersRemove {
    /// Filter ID to remove
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
#[serde(deny_unknown_fields)]
pub struct TriggersList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersListResult {
    pub triggers: Vec<TriggerInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TriggersAdd {
    /// DSL filter expression that activates the trigger
    pub filter: String,
    /// Number of messages to capture before the triggering event (default: 500)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_window: Option<u32>,
    /// Number of messages to capture after the triggering event (default: 200)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_window: Option<u32>,
    /// Number of context entries to include in the notification (default: 5)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_context: Option<u32>,
    /// Human-readable description
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
#[serde(deny_unknown_fields)]
pub struct TriggersEdit {
    /// Trigger ID to edit
    pub id: u32,
    /// New DSL filter expression
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// New pre-window size
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_window: Option<u32>,
    /// New post-window size
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_window: Option<u32>,
    /// New notify-context size
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_context: Option<u32>,
    /// New description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Set or clear fire-once. Absent leaves it as it is.
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
#[serde(deny_unknown_fields)]
pub struct TriggersRemove {
    /// Trigger ID to remove
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
#[serde(deny_unknown_fields)]
pub struct TracesRecent {
    /// Max traces to return (default: 20)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// Span filter DSL expression
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
#[serde(deny_unknown_fields)]
pub struct TracesGet {
    /// 32-character hex trace ID
    pub trace_id: String,
    /// Include linked logs (default: true)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_logs: Option<bool>,
    /// Filter spans within the trace
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
#[serde(deny_unknown_fields)]
pub struct TracesSummary {
    /// 32-character hex trace ID
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
#[serde(deny_unknown_fields)]
pub struct TracesSlow {
    /// Duration threshold in milliseconds (default: 100)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_duration_ms: Option<f64>,
    /// Max results (default: 20)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// Additional span filter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// `"name"` aggregates results into `groups`; `"none"` (or `""`, or
    /// omitting this) returns individual spans in `spans`.
    ///
    /// **This was the one `group_by` that accepted any string**, with anything
    /// other than `"name"` quietly selecting the ungrouped arm — so a typo
    /// returned a different answer than the one asked for, silently. The
    /// argument for that was that a closed set would reject every valid call
    /// meaning "do not group", which held only while no spelling for it
    /// existed. `none` is that spelling, so the set closes (#7).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(extend("enum" = ["name", "none", "", null]))]
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
#[serde(deny_unknown_fields)]
pub struct TracesLogs {
    /// 32-character hex trace ID
    pub trace_id: String,
    /// Additional log filter DSL
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
#[serde(deny_unknown_fields)]
pub struct SpansContext {
    /// Span sequence number
    pub seq: u64,
    /// Spans before (default: 5)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    /// Spans after (default: 5)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SpansContextResult {
    pub spans: Vec<SpanEntry>,
    pub count: usize,
}

/// Every span whose seq falls in an inclusive range.
///
/// **The span store had no seq-ranged read at all.** Its whole surface was
/// `get_trace`, `slow_spans`, `for_each_matching`, `duration_by_name`,
/// `recent_traces` and `spans.context` — and `spans.context` counts *positions
/// in the ring*, not seqs, so it cannot answer "what spans belong to this
/// window". A capture that pairs logs with the spans covering the same interval
/// needs one, which is what this is.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpansExport {
    /// Lowest seq to export, **inclusive**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_seq: Option<u64>,
    /// Highest seq to export, **inclusive**. Same convention as `logs.export`:
    /// a window of "this span, none before and none after" must contain it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_seq: Option<u64>,
    /// Maximum spans to return. Everything matching is still counted, so
    /// `capped` and `matched` stay truthful when this bites.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// Additional span filter, ANDed with the range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SpansExportResult {
    pub spans: Vec<SpanEntry>,
    /// How many are in `spans`.
    pub count: usize,
    /// How many matched the range in total. Differs from `count` only when
    /// `capped`, and it is the figure that says by how much.
    #[serde(default)]
    pub matched: usize,
    /// `count` stopped this short of everything that matched.
    #[serde(default)]
    pub capped: bool,
    /// **The span ring's own retention, not the log pipeline's.** The two
    /// stores share a seq axis but evict independently, so a log window's
    /// verdict says nothing about whether the spans over that same range
    /// survived. One verdict cannot honestly cover two stores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_oldest_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_newest_seq: Option<u64>,
    /// The requested range began before the span ring's oldest retained span.
    #[serde(default)]
    pub truncated: bool,
    /// How many seqs lie between the requested start and the oldest span still
    /// held — an upper bound on what was lost to eviction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evicted_before_window: Option<u64>,
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
    /// Sample standard deviation (Bessel-corrected, `n-1`) over exactly the
    /// durations the percentiles beside it were computed from — the same
    /// population, after any warm-up cut.
    ///
    /// Absent below two records, where it is undefined rather than zero. Also
    /// absent on `group_by: path` rows, which hold a count and a self-time sum
    /// rather than the per-span durations either figure needs; there, absence
    /// says nothing about how many records the row covers.
    ///
    /// The `n-1` form because at the sizes this serves the population form is
    /// about 18% smaller at three records (it is smaller by `sqrt((n-1)/n)`).
    /// Note that is the gap between the two *formulas*: both still estimate the
    /// true spread from very few samples, and at three records the estimate is
    /// itself uncertain to roughly ±45%. This is a description of the observed
    /// spread, not a significance test — comparing two three-run means properly
    /// needs a difference of roughly 2.3 standard deviations, not one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stddev_ms: Option<f64>,
    /// Every retained duration, in **arrival order**, when the sample is
    /// complete and holds at most 50 records.
    ///
    /// Arrival order rather than sorted: a reader can sort these but could not
    /// unsort them, and the order carries drift across a run — first-call
    /// effects, a cache filling — which every other figure here discards.
    /// `durations_ms[0]` is the FIRST duration, not the smallest.
    ///
    /// Absent when the sample was truncated (a prefix of a run presented as its
    /// durations would invite exactly the per-value reasoning a biased subset
    /// cannot support), and absent above 50 records. On a live read `suppressed`
    /// names which of the two applies.
    ///
    /// Two further absences carry no `suppressed` entry at all, because their
    /// responses have no such channel. A recorded run read by label serves the
    /// projection as it stood when the snapshot was taken; and any response that
    /// LISTS runs — `collectors.history`, and the `collectors.snapshot` reply —
    /// drops the durations whatever their size, because a bound written for one
    /// block would otherwise multiply by the number of runs returned. Read a
    /// single run by label to get them.
    ///
    /// Only the top-level `sampled` block populates this. Group rows leave it
    /// absent regardless of their size: it is the population the headline
    /// figures describe, and per-row lists would multiply it by `top_n`.
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
#[serde(deny_unknown_fields)]
pub struct ThresholdSpec {
    /// `count`, `total_ms`, `avg_ms`, `error_count` or `error_rate_pct`.
    ///
    /// **Percentiles are deliberately absent.** A rolling percentile needs a
    /// duration sketch per bucket, and this design's per-collector memory is
    /// bounded precisely so a collector cannot grow without limit. Use `avg_ms`
    /// for the guard and read the real percentiles with `collectors.get`.
    #[schemars(extend(
        "enum" = ["count", "total_ms", "avg_ms", "error_count", "error_rate_pct"]
    ))]
    pub metric: String,
    /// Restrict to one group value. Requires the collector to declare
    /// `group_keys`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// `gt`, `gte`, `lt` or `lte` — and the symbolic spellings `>`, `>=`, `<`
    /// and `<=`, which the parser has always accepted (`threshold.rs:120`) and
    /// this doc comment used to omit. The word forms are canonical; the symbols
    /// are listed because a schema narrower than the daemon rejects calls the
    /// daemon would have served.
    #[schemars(extend("enum" = ["gt", ">", "gte", ">=", "lt", "<", "lte", "<="]))]
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
#[serde(deny_unknown_fields)]
pub struct CollectorsAdd {
    /// Collector name. Letters, digits, '_' and '-' only.
    pub name: String,
    /// Span filter DSL. Matched against every span in the domain this session
    /// is bound to *now*; the collector stays pinned to that domain afterwards.
    pub filter: String,
    /// `scalar`, `timing`, or `tree` (default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("enum" = ["scalar", "timing", "tree", null]))]
    pub level: Option<String>,
    /// Span attributes to split the numbers by. Values are read directly, so
    /// booleans and numbers work — unlike the filter path, which only sees
    /// strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_keys: Option<Vec<String>>,
    /// Why this collector exists. Returned with every read, so a result found
    /// later still carries its context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Per-collector retained-sample budget in bytes (default 64 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sample_bytes: Option<u64>,
    /// A rolling guard over this collector's matched spans (§8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<ThresholdSpec>,
    /// Provenance to record while arming: `domain_data.update` entries, applied
    /// to this call's domain. Same validation, same per-entry outcomes, same
    /// `/logmon/` guard, same caps — one implementation, not a second store.
    ///
    /// A shorthand and nothing more. It is **not** persisted with the
    /// collector, does not appear in its definition, and `collectors.edit` does
    /// not touch it, because the registry already holds it. It exists because
    /// recording what the project was *while you are already describing what
    /// you are measuring* costs one parameter, where a separate call costs a
    /// decision — and the decision is the part that does not get made.
    ///
    /// A `@`-sigilled key is **rejected**: the sigil scopes a fact to one
    /// document, and arming a collector writes none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Vec<DomainDataEntry>>,
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
    /// One outcome per `data` entry, in the order sent — exactly what
    /// `domain_data.update` returns, so an agent learns one vocabulary rather
    /// than two. Absent when no `data` was sent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_outcomes: Vec<DomainDataOutcome>,
    /// Set when `data` reached memory but not disk. Named rather than
    /// swallowed, for the reason `domain_data.update` names it: provenance that
    /// will not survive a restart, reported as success, is the exact failure
    /// the registry exists to prevent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_persist_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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

/// `collectors.reset` — zero a collector and **discard** the window.
///
/// A one-field request type still earns its place: without it the method has no
/// schema, and a tool whose parameters nothing describes cannot be checked
/// against the shim, generated from, or published in a manifest.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CollectorsReset {
    /// The collector to zero.
    pub name: String,
}

/// `collectors.remove` — disarm and delete, taking its recorded runs with it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CollectorsRemove {
    /// The collector to remove.
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CollectorsGet {
    /// The collector to read.
    pub name: String,
    /// Read a recorded snapshot by label instead of the live window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// `name`, `group`, `trace`, or `path`. `none` and `""` are also accepted
    /// and mean the ungrouped overall figures — the same as omitting it
    /// (`project.rs:74`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("enum" = ["name", "group", "trace", "path", "none", "", null]))]
    pub group_by: Option<String>,
    /// Exclude spans starting within this many ms of the first matched span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_warmup_ms: Option<f64>,
    /// Rows returned in the breakdown (default 20).
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
#[serde(deny_unknown_fields)]
pub struct CollectorsEdit {
    /// The collector to change.
    pub name: String,
    /// Free — changes nothing about what is collected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Any of the fields below DISCARDS the live window. Recorded snapshots are
    /// untouched and each keeps the definition it was taken under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// `scalar`, `timing` or `tree`. Permitted in **both** directions: a `tree`
    /// collector heading for the sample cap mid-run can drop to `timing` for
    /// 2.5× the records, which is the only remedy left once the daemon-wide
    /// reservation is exhausted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("enum" = ["scalar", "timing", "tree", null]))]
    pub level: Option<String>,
    /// Replace the declared group keys. A structural change: it zeroes the live
    /// window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_keys: Option<Vec<String>>,
    /// Replace the per-collector sample budget. A structural change: it zeroes
    /// the live window.
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
#[serde(deny_unknown_fields)]
pub struct CollectorsSnapshot {
    /// The collector to record.
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
    /// Keep the per-span-name breakdown in the recorded run. Default true. Per-
    /// axis breakdowns are what a later comparison splits by, and they are
    /// dropped on the way to disk when this is false — so the choice is made
    /// here, once, and cannot be revisited from a recorded run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_name: Option<bool>,
    /// Keep the per-group-key breakdown in the recorded run. Default true.
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
#[serde(deny_unknown_fields)]
pub struct CollectorsHistory {
    /// The collector whose recorded runs to list.
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
#[serde(deny_unknown_fields)]
pub struct CollectorsDiff {
    /// Baseline arm: "<collector>" for the live window, "<collector>@<label>"
    /// for one recorded run, or "<collector>@*" for every recorded run merged.
    pub a: String,
    /// The arm to compare against the baseline, same syntax.
    pub b: String,
    /// `name` or `group`. Not `trace` or `path`: both are projected from
    /// per-span records, and a recorded run does not retain those. `none` and
    /// `""` are accepted for the ungrouped comparison, which is also the
    /// default when this is omitted (`diff.rs:77`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("enum" = ["name", "group", "none", "", null]))]
    pub group_by: Option<String>,
    /// Rows returned in the breakdown (default 20).
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
#[serde(deny_unknown_fields)]
pub struct CollectorsDocument {
    /// The arms to document. The **first is the baseline**; every other one is
    /// compared against it. Same syntax as `collectors.diff`.
    pub names: Vec<String>,
    /// `md` (default), `json`, or `folded`. `markdown` and `""` are accepted
    /// for `md`, and `collapsed` for `folded` (`document.rs:57`) — aliases the
    /// error message does not name but the parser has always taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend(
        "enum" = ["md", "markdown", "", "json", "folded", "collapsed", null]
    ))]
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
    /// `name` (default) or `group`. `none` and `""` are accepted for the
    /// ungrouped document (`diff.rs:77`). Note the default differs from
    /// `collectors.diff`: omitting it here means `name`, not ungrouped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("enum" = ["name", "group", "none", "", null]))]
    pub group_by: Option<String>,
    /// Rows in the ranked table (default 15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<u64>,
    /// Document arms whose filters matched different populations. Off by
    /// default: the comparison is then between different things.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_mismatch: Option<bool>,
    /// Document arms whose ingest dropped spans. Off by default: the missing
    /// spans are not random.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_lossy: Option<bool>,
    /// Document arms whose sample tier hit its cap. Off by default: the tail is
    /// the part that was cut.
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
#[serde(deny_unknown_fields)]
pub struct TracesProfile {
    /// Span filter DSL. Default "ALL".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// `name`, `group`, `trace`, or `path`; `none`, `""` or omitted for the
    /// ungrouped figures. The same axes `collectors.get` takes — both read
    /// through `profile_options` (`rpc_handler.rs:2314`), so they cannot drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("enum" = ["name", "group", "trace", "path", "none", "", null]))]
    pub group_by: Option<String>,
    /// Span attributes to split by. Required for group_by "group".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_keys: Option<Vec<String>>,
    /// Exclude spans starting within this many ms of the first matched span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_warmup_ms: Option<f64>,
    /// Rows returned in the breakdown (default 20).
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
    /// 20 of 20" from "the top 20 of 900".
    ///
    /// Absent when no grouping was performed — either none was asked for, or
    /// the one asked for was refused (see `suppressed`). A refused grouping has
    /// no denominator, and `0` there would read as "this run touched nothing"
    /// rather than "we did not look".
    ///
    /// Counts every key this one arm's axis holds, `__overflow__` included when
    /// a cardinality cap folded values into it. `CollectorsDiffResult` carries a
    /// field of the same name that counts something else — the *comparable* keys
    /// across the union of two arms, `__overflow__` excluded — so the two are
    /// not expected to reconcile and can differ by any amount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups_total: Option<usize>,
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
#[serde(deny_unknown_fields)]
pub struct BookmarksAdd {
    /// Bookmark name (alphanumerics, '-', '_'; max 64 chars). Will be qualified
    /// with the calling session's name automatically.
    pub name: String,
    /// Caller-chosen anchor seq. Defaults to the daemon's current seq counter
    /// (i.e., "right now") when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_seq: Option<u64>,
    /// Optional caller-supplied note retained for human display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// If true, overwrite an existing bookmark with the same qualified name.
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
#[serde(deny_unknown_fields)]
pub struct BookmarksList {
    /// Optional: filter to bookmarks created by this session name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksListResult {
    pub bookmarks: Vec<BookmarkInfo>,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BookmarksRemove {
    /// Bare name (resolved against current session) or qualified
    /// "session/name".
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksRemoveResult {
    pub removed: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SessionsList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionsListResult {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionsDrop {
    /// Name of the session to drop
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionsDropResult {
    pub dropped: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// This broker's version. Additive; an older daemon that omits it
    /// deserializes as `""`.
    #[serde(default)]
    pub broker_version: String,
    /// The MCP tools a shim built at this broker's version exposes
    /// ([`crate::mcp_tools::TOOLS`]). A client holding fewer is out of date.
    ///
    /// Mirrored here — not left to the untyped passthrough — because this type
    /// is deserialized and re-serialized on the typed SDK path, so a field the
    /// daemon emits but this struct lacks is **silently dropped for every typed
    /// caller**. That has happened before in this crate; see the note on
    /// [`TriggersEditResult::post_remaining`]. `verify-schema` cannot catch it:
    /// it checks schema-against-Rust, not daemon-JSON-against-Rust.
    #[serde(default)]
    pub broker_tools: Vec<String>,
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
#[serde(deny_unknown_fields)]
pub struct DomainsCreate {
    /// Domain name — the isolation key.
    pub name: String,
    /// GELF UDP+TCP port. Omitted → auto-allocate; `0` → disable GELF for this
    /// domain; otherwise bind exactly this port (error if held by another domain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gelf_port: Option<u16>,
    /// OTLP gRPC port. Omitted → auto-allocate; 0 → disable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_grpc_port: Option<u16>,
    /// OTLP HTTP port. Omitted → auto-allocate; 0 → disable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_http_port: Option<u16>,
    /// Log ring capacity (default: the daemon's configured size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_buffer_size: Option<usize>,
    /// Span ring capacity (default: the daemon's configured size).
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
#[serde(deny_unknown_fields)]
pub struct DomainsDelete {
    /// Name of the domain to delete. Refuses config-declared domains (incl.
    /// 'default').
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainsDeleteResult {
    pub name: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainsList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainsListResult {
    pub domains: Vec<DomainInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainsUse {
    /// Domain to bind this session to for subsequent queries and notifications.
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
#[serde(deny_unknown_fields)]
pub struct DomainsClear {}

/// `sessions.rename` — re-key the calling session to a meaningful name
/// (`<Project>-Main-<short8>` / `<Project>-tN-<branch>`, spec 2026-07-17),
/// preserving all state. Errors with "already connected" when the target name
/// is held by a LIVE session — the one-conversation-per-lane invariant.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionsRename {
    /// New session name: <Project>-Main-<short8> or <Project>-tN-<branch> ('/'
    /// sanitized to '-'; alphanumerics, '-' and '_' only).
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionsRenameResult {
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

// ---------------------------------------------------------------------------
// domain_data.* — the per-domain provenance registry (case-documents spec §3)
// ---------------------------------------------------------------------------

/// One requested change to the registry.
///
/// **Absent ≡ `null` ≡ unchanged, for both `value` and `ttl`**, and the two
/// share one rule deliberately: two optional fields on one object with opposite
/// absent semantics is the footgun this surface exists to avoid, and a client
/// serialising a missing field as `null` would otherwise mean the opposite of
/// what it sent. Erasure is `domain_data.remove`, never `value: null`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainDataEntry {
    /// A registry path — `/Build/commit`. A leading `@` scopes the fact to one
    /// case document instead of the domain, and is accepted only where a
    /// document exists to receive it.
    pub path: String,
    /// Present → set. Absent → validate only, which never creates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// A duration literal (`30s`, `5m`, `2h`, `7d`, `4w`), or `false` to clear
    /// one in place. Absent leaves any existing lifetime untouched.
    ///
    /// `false` rather than `null` carries the clear because `null` is already
    /// spoken for above. This is the opposite convention to
    /// `CollectorsEdit.threshold`, where `null` removes — noted because the two
    /// live in one daemon and the difference is deliberate rather than drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainDataUpdate {
    /// Entries to record. Each is `{path, value?, ttl?}`. **Value present** →
    /// set it. **Value absent** → *validate*: confirm what is already there,
    /// moving only its confirmation time. A key-only entry never creates —
    /// recording a key with no value would be a guess. `ttl` is `30s` / `5m` /
    /// `2h` / `7d` / `4w`, or `false` to clear one. Absent leaves any existing
    /// lifetime alone, exactly as an absent value does, so restating a value
    /// cannot silently drop its lifetime.
    pub entries: Vec<DomainDataEntry>,
}

/// What happened to one entry.
///
/// `outcome` is one of `created`, `updated`, `validated`, `unknown`,
/// `rejected`. The last two carry a discriminant rather than prose, so a caller
/// can branch instead of string-matching a sentence.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainDataOutcome {
    /// The key as the caller spelled it, sigil included.
    pub path: String,
    pub outcome: String,
    /// `never_set` | `no_registry` | `undetermined` — present on `unknown`.
    ///
    /// Three, not two: `/logmon/first_seen` is written identically for a
    /// brand-new domain and for one whose file was lost, so from the registry
    /// alone those are not distinguishable. `undetermined` is the honest
    /// default and `no_registry` is claimed only with a quarantine artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// `malformed_path` | `path_too_long` | `value_too_long` |
    /// `reserved_prefix` | `sigil_not_allowed_here` | `malformed_ttl` |
    /// `registry_full` — present on `rejected`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainDataUpdateResult {
    pub outcomes: Vec<DomainDataOutcome>,
    /// Set when the change is in memory but did not reach disk. Named rather
    /// than swallowed: the caller's provenance will not survive a restart, and
    /// silence there is exactly the failure this registry exists to prevent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persist_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainDataRemove {
    /// Prefix patterns, matched on **segment boundaries** — `/Versions` matches
    /// `/Versions/ht_server` but `/Ver` matches neither. There is no undo.
    pub patterns: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainDataRemoveOutcome {
    pub pattern: String,
    pub removed: usize,
    /// Present when the pattern was refused — notably `reserved_prefix`, which
    /// covers the bare `/logmon` as well as `/logmon/version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainDataRemoveResult {
    pub outcomes: Vec<DomainDataRemoveOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persist_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainDataGet {
    /// Narrow to a subtree, on segment boundaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Return only keys last confirmed before this many seconds ago — the
    /// "what is stale" question, as a filter over `get` rather than a tool of
    /// its own, so there are not two surfaces to keep in step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validated_before_secs: Option<u64>,
}

/// One key as read back.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainDataKey {
    pub path: String,
    pub value: String,
    /// When THIS VALUE came into force. Moves only when the value changes, so a
    /// value confirmed daily for a month still reports the month.
    pub created_at: String,
    /// When someone last confirmed it. Invariant: `>= created_at`.
    pub validated_at: String,
    pub age_secs: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    /// `true`/`false` only when a `ttl` was stated. **Absent means unknown, not
    /// fresh** — a key without a lifetime reports its age and no verdict, ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expired: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DomainDataGetResult {
    pub domain: String,
    pub keys: Vec<DomainDataKey>,
    /// Total keys in the registry before `prefix`/`validated_before_secs`
    /// narrowed it, so a filtered read cannot be mistaken for an empty one.
    pub total: usize,
    /// Coverage against the recommended core set, named rather than counted:
    /// which of `/Build/commit`, `/Build/profile`, `/Action` are missing.
    pub missing_core: Vec<String>,
}

// =============================================================================
// cases.*
// =============================================================================

/// Where a capture is centred — **tagged**, not one string the daemon sniffs.
///
/// Sniffing misfires on a bookmark named `12345` or one spelled as 32 hex
/// characters, and the failure is silent: you get a document anchored somewhere
/// else. The anchor entry's message is the document's headline, so a wrong
/// anchor is a wrong document rather than a wrong parameter.
///
/// Exactly one field must be set.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaseAnchor {
    /// A stored entry's sequence number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// A bookmark name, resolved against the **calling session** exactly as
    /// `b>=name` resolves it — use `<session>/<name>` for another session's.
    /// Anchors on the first stored entry **strictly after** the mark, since a
    /// bookmark marks a boundary rather than a record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bookmark: Option<String>,
    /// A trace id in hex. Anchors on the **earliest by seq** of its entries,
    /// and the document says how many there were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CasesCreate {
    /// Why this capture exists. Required: a manual case that cannot say why it
    /// was taken has no provenance, and unlike a watch there is no filter
    /// standing in for one.
    pub reason: String,
    pub anchor: CaseAnchor,
    /// **Absolute** directory the three files are written to. A relative path
    /// is rejected rather than resolved: the broker runs as a service, so it
    /// would resolve against the broker's working directory rather than the
    /// caller's.
    ///
    /// The daemon writes, rather than returning the archive over RPC, because a
    /// case carries several hundred kilobytes of JSONL and returning it would
    /// put the whole thing in the model's context — the outcome the
    /// document/logdata split exists to prevent.
    pub dir: String,
    /// Filename prefix. Falls back to `/case-name` in the registry, then to the
    /// domain name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Registry entries, exactly as `domain_data.update` takes them. A leading
    /// `@` scopes a key to **this document** instead of the domain — how a
    /// capturer records something true of this incident rather than of the
    /// domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Vec<DomainDataEntry>>,
    /// Stored records to capture before the anchor. Counts of **records**, not
    /// seq distances: one counter feeds both the log and span stores, so a
    /// 200-seq range holds an unpredictable number of logs. Default 350, capped
    /// at 5000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u32>,
    /// Stored records to capture after the anchor. Default 350, capped at 5000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u32>,
}

/// One written evidence file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CaseFile {
    /// Data records, excluding the format header line.
    pub records: u64,
    /// File size including the header.
    pub bytes: u64,
}

/// Something the caller should know that did not stop the capture.
///
/// Typed rather than a prose list, because capture gaps, provenance
/// observations and write problems are three different things and a caller
/// acting on them should not have to tell them apart by reading English.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CaseNote {
    /// `capture_gap`, `provenance`, `truncated` or `write`.
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CasesCreateResult {
    /// `checkout-hang-260731-021530` — the stem all three files share.
    pub stem: String,
    /// What was actually written, document first. Not what was intended.
    pub paths: Vec<String>,
    /// The §4.2 verdict for the captured window, on the wire as well as in the
    /// document — the document's most important correctness property must not
    /// be reachable only by parsing markdown.
    pub verdict: EvidenceVerdict,
    /// The document is the artifact whose size can run away, so it is the one
    /// that gets measured alongside the two that cannot.
    pub document_bytes: u64,
    pub logdata: CaseFile,
    pub spandata: CaseFile,
    pub notes: Vec<CaseNote>,
    /// Per-entry outcomes for `data`, in the order sent — the same call as
    /// `domain_data.update`, with **one value it never emits**: `scoped`, for a
    /// key carrying the `@` sigil, which went to this document instead of the
    /// registry. A closed-set reader written against `domain_data.update` needs
    /// that arm added.
    #[serde(default)]
    pub data_outcomes: Vec<DomainDataOutcome>,
    /// Set when the registry could not be made durable. The capture still
    /// happened and the files are still on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_persist_error: Option<String>,
}
