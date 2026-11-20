//! Versioned, read-only views of a domain's test-log activity.
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const TEST_LOG_SCHEMA_VERSION: u32 = 1;

fn supported_version<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let version = u32::deserialize(d)?;
    if version == TEST_LOG_SCHEMA_VERSION {
        Ok(version)
    } else {
        Err(serde::de::Error::custom(format!(
            "unsupported test-log schema version {version}"
        )))
    }
}

// Wire contract: these keys are present even when their values are unknown.
fn nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(d)
}

fn nullable_schema<T: JsonSchema>(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    Option::<T>::json_schema(generator)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TestLogStatus {
    /// Existing domain to inspect. Defaults to the caller's bound domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Read this retained run, including a closed or sealed run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TestLogList {
    /// Existing domain to inspect. Defaults to the caller's bound domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestLogRunState {
    Open,
    Closing,
    Closed,
    Sealed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogLane {
    pub run_id: String,
    pub state: TestLogRunState,
    pub in_flight: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub struct TestLogVerdictCounts {
    pub ok: u64,
    pub fail: u64,
    pub timeout: u64,
    pub aborted: u64,
    pub no_verdict: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogExecutionCounts {
    #[serde(deserialize_with = "nullable")]
    #[schemars(schema_with = "nullable_schema::<u64>")]
    pub planned: Option<u64>,
    pub started: u64,
    pub ended: u64,
    pub in_flight: u64,
    pub by_verdict: TestLogVerdictCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogCaseCounts {
    pub requested: u64,
    pub ready_executions: u64,
    pub distinct_cases_written: u64,
    pub pending: u64,
    pub failed: u64,
    pub skipped: u64,
    /// Legacy counter retained for older clients. The limit now produces minimal cases.
    pub refused_by_max_failures: u64,
    #[serde(default)]
    pub minimal_by_failure_limit: u64,
    pub max_failures: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogSummaryCounts {
    pub r#final: u64,
    pub pending: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogRunSummary {
    pub run_id: String,
    pub state: TestLogRunState,
    pub began_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
    pub last_completion_at: Option<DateTime<Utc>>,
    pub fields: BTreeMap<String, Value>,
    pub executions: TestLogExecutionCounts,
    pub cases: TestLogCaseCounts,
    pub summaries: TestLogSummaryCounts,
    pub replayed_ends: u64,
    #[serde(default)]
    pub faults: TestLogFaultCounts,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TestLogFaultCounts {
    pub excessive_telemetry: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogRunningExecution {
    pub name: String,
    pub execution_id: String,
    #[serde(deserialize_with = "nullable")]
    #[schemars(schema_with = "nullable_schema::<String>")]
    pub trace_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub elapsed_ms: u64,
    #[serde(deserialize_with = "nullable")]
    #[schemars(schema_with = "nullable_schema::<u64>")]
    pub deadline_overdue_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogRecentFailure {
    pub execution_id: String,
    pub name: String,
    pub failed_test: Option<String>,
    pub verdict: String,
    pub message: Option<String>,
    pub ended_at: DateTime<Utc>,
    pub case_status: Option<String>,
    pub case_path: Option<String>,
    pub rerun_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogCaseFile {
    pub path: String,
    pub execution_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogDetailedCases {
    #[serde(flatten)]
    pub counts: TestLogCaseCounts,
    pub files: Vec<TestLogCaseFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogJournalHealth {
    #[serde(deserialize_with = "nullable")]
    #[schemars(schema_with = "nullable_schema::<Value>")]
    pub write_error: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogRunDetail {
    pub run_id: String,
    pub state: TestLogRunState,
    pub began_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
    pub last_completion_at: Option<DateTime<Utc>>,
    pub elapsed_ms: u64,
    pub jsonl_path: String,
    pub fields: BTreeMap<String, Value>,
    pub executions: TestLogExecutionCounts,
    pub longest_in_flight: Vec<TestLogRunningExecution>,
    pub recent_failures_truncated: bool,
    pub recent_failures: Vec<TestLogRecentFailure>,
    pub cases: TestLogDetailedCases,
    pub summaries: TestLogSummaryCounts,
    pub replayed_ends: u64,
    pub journal: TestLogJournalHealth,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogQueryError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestLogQueryErrorResult {
    #[serde(deserialize_with = "supported_version")]
    #[schemars(extend("const" = TEST_LOG_SCHEMA_VERSION))]
    pub schema_version: u32,
    pub error: TestLogQueryError,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TestLogStatusResult {
    Error(TestLogQueryErrorResult),
    Success {
        #[serde(deserialize_with = "supported_version")]
        #[schemars(extend("const" = TEST_LOG_SCHEMA_VERSION))]
        schema_version: u32,
        domain: String,
        #[serde(deserialize_with = "nullable")]
        #[schemars(schema_with = "nullable_schema::<Box<TestLogRunDetail>>")]
        test_log: Option<Box<TestLogRunDetail>>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TestLogListResult {
    Error(TestLogQueryErrorResult),
    Success {
        #[serde(deserialize_with = "supported_version")]
        #[schemars(extend("const" = TEST_LOG_SCHEMA_VERSION))]
        schema_version: u32,
        domain: String,
        runs: Vec<TestLogRunSummary>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json};

    #[test]
    fn accepts_only_supported_progress_envelopes_including_explicit_idle() {
        let mut idle = json!({"schema_version":1,"domain":"lane","test_log":null,"future":true});
        assert!(matches!(
            from_value::<TestLogStatusResult>(idle.clone()),
            Ok(TestLogStatusResult::Success { test_log: None, .. })
        ));
        idle.as_object_mut().unwrap().remove("test_log");
        assert!(from_value::<TestLogStatusResult>(idle).is_err());
        for version in [None, Some(2)] {
            let mut list = json!({"domain":"lane","runs":[]});
            if let Some(v) = version {
                list["schema_version"] = json!(v);
            }
            assert!(from_value::<TestLogListResult>(list).is_err());
        }
    }

    #[test]
    fn accepts_nullable_plan_and_requires_every_verdict_bucket() {
        let mut counts = json!({"planned":null,"started":0,"ended":0,"in_flight":0,
            "by_verdict":{"OK":0,"FAIL":0,"TIMEOUT":0,"ABORTED":0,"NO_VERDICT":0}});
        assert_eq!(
            from_value::<TestLogExecutionCounts>(counts.clone())
                .unwrap()
                .planned,
            None
        );
        counts["planned"] = json!(0);
        assert_eq!(
            from_value::<TestLogExecutionCounts>(counts.clone())
                .unwrap()
                .planned,
            Some(0)
        );
        counts["by_verdict"]
            .as_object_mut()
            .unwrap()
            .remove("NO_VERDICT");
        assert!(from_value::<TestLogExecutionCounts>(counts).is_err());
    }
}
