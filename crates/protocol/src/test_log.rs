//! Types shared by the daemon, nightly runner, SDK, and generated CLI.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum TestVerdict {
    Ok,
    Fail,
    Timeout,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogSelector {
    /// Field to match on logs and spans, such as trace_id.
    pub field: String,
    /// Exact selector value identifying this execution.
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogBegin {
    /// Identity of this run, unique across invocations.
    pub run_id: String,
    /// Planned executions. Set it on begin or repeat begin to fill an unknown count
    /// while the run is open. Zero is an explicitly empty plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_executions: Option<u64>,
    /// Absolute JSONL path chosen by the run owner.
    pub jsonl_path: String,
    /// Run-scoped provenance copied into each case.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, Value>,
    /// Write loose case files in a per-execution directory.
    #[serde(default)]
    pub expanded_cases: bool,
    /// IANA timezone for human-readable reports (UTC by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_timezone: Option<String>,
    /// Missing-end deadline in milliseconds, default 600000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_timeout_ms: Option<u64>,
    /// Time to collect late telemetry after a verdict, default 10000 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_period_ms: Option<u64>,
    /// Caller acknowledgement budget for live start/end in milliseconds, default 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_ack_timeout_ms: Option<u64>,
    /// Override the number of test failures eligible for full cases. Otherwise use
    /// 10% of the planned executions (20 to 300), or 50 if the plan is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_failures: Option<u64>,
    /// Number of OK isolated reruns eligible for cases. Failed reruns use the
    /// same policy as other failed tests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rerun_cases: Option<u64>,
    /// Maximum slow-pass admissions before refusing further slow cases, default 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_slow_pass_cases: Option<u64>,
    /// Span attributes whose distinct values should be retained independently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collect_distinct: Vec<String>,
    /// Slow-pass policy: opt-in or opt-out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow_pass: Option<String>,
    /// Default condition when slow passes are enabled by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow_if: Option<String>,
    /// Stable percentage of chain names selected for detailed summaries (0 to 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_sample_percent: Option<f64>,
    /// Per-selector share of each ring's configured maximum, or of its size at begin
    /// when no maximum is set. Defaults to 25%, with a minimum of one record per ring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_limit_percent: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogStart {
    /// Identity of the active run.
    pub run_id: String,
    /// Identity of this execution; a retry of the same marker reuses it.
    pub execution_id: String,
    /// Stable fully qualified test or chain name.
    pub name: String,
    /// Field/value pair attributing telemetry to this execution.
    pub selector: TestLogSelector,
    /// Original start timestamp with UTC offset.
    pub started_at: DateTime<Utc>,
    /// Expected elapsed duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_ms: Option<u64>,
    /// Positive per-execution missing-end deadline overriding the run default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_timeout_ms: Option<u64>,
    /// Slow-pass condition for this execution, or off to disable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow_if: Option<String>,
    /// Original execution ID if this is an isolated rerun.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerun_of: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogEnd {
    /// Identity of the active run.
    pub run_id: String,
    /// Identity of the execution whose outcome is being reported.
    pub execution_id: String,
    /// Original stable test or chain name, required for missing-start recovery.
    pub name: String,
    /// Original field/value pair used to backfill when start was unobserved.
    pub selector: TestLogSelector,
    /// Start time from the runner, retained on replay.
    pub started_at: DateTime<Utc>,
    /// End time from the runner, retained on replay.
    pub ended_at: DateTime<Utc>,
    /// Runner's original execution outcome.
    pub verdict: TestVerdict,
    /// Original execution ID for an isolated rerun.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerun_of: Option<String>,
    /// Additional evidence; conventional keys include message, failed_test, file and line.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogMachineLoad {
    /// Identity of the active run.
    pub run_id: String,
    /// External sampler identity. Older callers default to machine-load.
    #[serde(default = "default_machine_kind")]
    pub kind: String,
    /// Source timestamp for the machine sample.
    pub ts: DateTime<Utc>,
    /// Name of the sampled host.
    pub host: String,
    /// Sampler payload, including load or process details.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogClose {
    /// Identity of the run to close; repeating close returns the current state.
    pub run_id: String,
    /// Maximum time in milliseconds to await settled results; zero checks immediately.
    pub wait_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogSeal {
    /// Exact identity of the abandoned run; a successor cannot be sealed by a stale caller.
    pub run_id: String,
    /// Maximum milliseconds to wait for finalization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogReplay {
    /// Run to reconcile before closing.
    pub run_id: String,
    /// Unconfirmed end events, in order, at most 100 per request.
    pub events: Vec<TestLogEnd>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogTelemetryCounts {
    pub logs: u64,
    pub spans: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestLogTelemetryStream {
    Logs,
    Spans,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogTelemetryPeer {
    pub execution_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogTelemetryLimit {
    /// Stream that crossed first.
    pub kind: TestLogTelemetryStream,
    /// Number of records that stream could admit before crossing.
    pub limit: u64,
    /// Ring capacity used to calculate the limit.
    pub ring_size: u64,
    /// Other executions registered under this selector value.
    pub shared_with: Vec<TestLogTelemetryPeer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogAck {
    /// Daemon decision: accepted, already_applied or refused.
    pub status: String,
    /// Machine-readable refusal reason, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Run addressed by this operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Execution addressed by this operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    /// Capture admission decision or case state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_status: Option<String>,
    /// Evidence size when a case is admitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_size: Option<String>,
    /// Capture constraint or policy that selected the case size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_detail: Option<String>,
    /// Telemetry fault independent of the runner’s verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<String>,
    /// Limit and other executions sharing the selector that crossed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_limit: Option<TestLogTelemetryLimit>,
    /// Refused telemetry by stream, separate from domain loss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused_telemetry: Option<TestLogTelemetryCounts>,
    /// Records missing from the configured tail window by stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_tail_shortened: Option<TestLogTelemetryCounts>,
    /// Bytes charged to the run’s shared tail budget when the execution ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_tail_pool_bytes: Option<u64>,
    /// Journal-relative path to the queued tail file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_tail_path: Option<String>,
    /// Tail error known when the acknowledgement was produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_tail_error: Option<String>,
    /// A degraded writer may accept the lifecycle operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_log_health: Option<String>,
    /// Additional decision details including active owner or file errors.
    #[serde(flatten)]
    pub details: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogPendingCase {
    /// Execution awaiting case finalization.
    pub execution_id: String,
    /// Stable name of the test or chain.
    pub name: String,
    /// Current pending state.
    pub state: String,
    /// Size selected for the case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_size: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogCloseResult {
    /// Operation disposition when supplied; refusals carry a code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Machine-readable refusal reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Run addressed by this request.
    pub run_id: String,
    /// settled or pending for a successfully initiated close.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalization: Option<String>,
    /// complete, degraded or incomplete, independent of finalization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence: Option<String>,
    /// Number of summaries awaiting finalization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_summaries: Option<u64>,
    /// Cases still awaiting a terminal decision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_cases: Vec<TestLogPendingCase>,
    /// Additional persistence diagnostics and recovery information.
    #[serde(flatten)]
    pub details: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogReplayResult {
    /// Disposition for the batch, or refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Machine-readable batch refusal reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Per-item applied, already_applied or refused decisions in input order.
    #[serde(default)]
    pub results: Vec<TestLogAck>,
}

fn default_machine_kind() -> String {
    "machine-load".into()
}

/// Source-time index, separately bracketed for each host and sampler.
pub struct MachineSeries<T> {
    streams: BTreeMap<(String, String), BTreeMap<(DateTime<Utc>, u64), T>>,
    count: u64,
}

impl<T> Default for MachineSeries<T> {
    fn default() -> Self {
        Self {
            streams: BTreeMap::new(),
            count: 0,
        }
    }
}
impl<T> MachineSeries<T> {
    pub fn insert(&mut self, host: &str, kind: &str, ts: DateTime<Utc>, sample: T) {
        self.count += 1;
        self.streams
            .entry((host.into(), kind.into()))
            .or_default()
            .insert((ts, self.count), sample);
    }

    pub fn window(&self, start: DateTime<Utc>, end: DateTime<Utc>, bracket: bool) -> Vec<&T> {
        let mut selected = Vec::new();
        let first = (start, 0);
        let last = (end.max(start), u64::MAX);
        for stream in self.streams.values() {
            if bracket {
                selected.extend(stream.range(..first).next_back());
            }
            selected.extend(stream.range(first..=last));
            if bracket {
                use std::ops::Bound::{Excluded, Unbounded};
                selected.extend(stream.range((Excluded(last), Unbounded)).next());
            }
        }
        selected.sort_unstable_by_key(|(key, _)| **key);
        selected.into_iter().map(|(_, v)| v).collect()
    }
}

/// Render the scheduling fields first and preserve the complete sampler payload.
pub fn machine_fields(fields: &Value) -> String {
    const FIRST: [&str; 5] = ["load", "idle", "runnable", "own_pri", "spawned"];
    let Some(map) = fields.as_object() else {
        return fields.to_string();
    };
    FIRST
        .into_iter()
        .filter_map(|key| map.get(key).map(|v| (key, v)))
        .chain(
            map.iter()
                .filter(|(k, _)| !FIRST.contains(&k.as_str()))
                .map(|(k, v)| (k.as_str(), v)),
        )
        .map(|(k, v)| format!("{}={v}", k.escape_debug()))
        .collect::<Vec<_>>()
        .join(" ")
}
