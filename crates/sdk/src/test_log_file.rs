//! Offline views over the durable test-log. Each summary revision, case outcome
//! and post-grace diagnostic belongs to the execution named in its journal row.
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Portable projection of an append-only JSONL test-log. An interrupted file
/// can contain active executions and an incomplete final line.
#[derive(Debug, Default, Serialize)]
pub struct TestLogFile {
    pub run_id: Option<String>,
    pub settled: bool,
    pub sealed: bool,
    pub closed_incomplete: bool,
    pub sampling_algorithm: Option<String>,
    pub summary_sample_percent: Option<f64>,
    pub reported_lost_records: u64,
    pub partial_last_line: bool,
    pub executions: BTreeMap<String, Map<String, Value>>,
}

impl TestLogFile {
    pub fn open(path: &Path) -> Result<Self> {
        Self::read(BufReader::new(File::open(path)?))
            .with_context(|| format!("reading {}", path.display()))
    }

    pub fn read(mut reader: impl BufRead) -> Result<Self> {
        let mut result = Self::default();
        let mut line = Vec::new();
        for index in 1.. {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let row: Map<String, Value> = match serde_json::from_slice(&line) {
                Ok(row) => row,
                Err(_) if line.last() != Some(&b'\n') => {
                    result.partial_last_line = true;
                    break;
                }
                Err(err) => return Err(err).with_context(|| format!("invalid JSONL line {index}")),
            };
            result.apply(row).with_context(|| format!("line {index}"))?;
        }
        Ok(result)
    }

    fn apply(&mut self, mut row: Map<String, Value>) -> Result<()> {
        let kind = row
            .remove("event")
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or_else(|| anyhow!("missing event name"))?;
        if let Some(id) = row.get("run_id").and_then(Value::as_str) {
            if self.run_id.get_or_insert_with(|| id.to_owned()) != id {
                bail!("multiple run IDs in the same journal");
            }
        }
        match kind.as_str() {
            "begin-test-log" => {
                self.sampling_algorithm = row
                    .get("sampling_algorithm")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.summary_sample_percent = Some(
                    row.get("settings")
                        .and_then(|v| v.get("summary_sample_percent"))
                        .and_then(Value::as_f64)
                        .unwrap_or(100.),
                );
            }
            "end-test-log" | "seal-test-log" => {
                self.sealed = kind == "seal-test-log";
                self.closed_incomplete = self.sealed
                    || row
                        .get("incomplete")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
            }
            "run-settled" => self.settled = true,
            "test-log-gap" => {
                let lost = row
                    .get("lost_records")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("gap record missing lost_records count"))?;
                self.reported_lost_records = self.reported_lost_records.max(lost);
            }
            _ => {}
        }
        let Some(id) = row.get("execution_id").and_then(Value::as_str) else {
            return Ok(());
        };
        let entry = self.executions.entry(id.to_owned()).or_default();
        match kind.as_str() {
            "start-test" | "end-test" | "incomplete" | "summary-final" => {
                // A timeout case can finish before the authoritative end arrives.
                // Preserve the independently recorded terminal case outcome.
                if matches!(
                    entry.get("case_status").and_then(Value::as_str),
                    Some("ready" | "error" | "evicted_for_priority")
                ) {
                    row.remove("case_status");
                }
                entry.extend(row);
                entry.insert("summary_final".into(), Value::Bool(kind == "summary-final"));
            }
            "case-ready" | "case-skipped" | "case-error" => {
                for key in ["case_status", "case_path"] {
                    if let Some(v) = row.remove(key) {
                        entry.insert(key.into(), v);
                    }
                }
                if let Some(message) = row.remove("message") {
                    entry.insert("case_error".into(), message);
                }
            }
            "late-telemetry" | "late-telemetry-counts" => {
                entry.insert("late_telemetry_observed".into(), Value::Bool(true));
                if kind == "late-telemetry-counts" {
                    entry.insert("late_telemetry_counts".into(), json!({
                        "logs":row.get("logs"), "spans":row.get("spans"), "as_of":row.get("as_of")
                    }));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Include file-level diagnostics when presenting a single execution or a comparison.
    pub fn journal_status(&self) -> Value {
        json!({"run_id":self.run_id,"settled":self.settled,"sealed":self.sealed,
            "closed_incomplete":self.closed_incomplete, "reported_lost_records":self.reported_lost_records,
            "partial_last_line":self.partial_last_line, "sampling_algorithm":self.sampling_algorithm,
            "summary_sample_percent":self.summary_sample_percent})
    }

    pub fn lookup(&self, execution_id: &str) -> Result<&Map<String, Value>> {
        self.executions
            .get(execution_id)
            .ok_or_else(|| anyhow!("unknown execution {execution_id}"))
    }

    pub fn lookup_name(&self, name: &str) -> Result<&Map<String, Value>> {
        let mut matching = self
            .executions
            .values()
            .filter(|r| r.get("name").and_then(Value::as_str) == Some(name));
        let first = matching
            .next()
            .ok_or_else(|| anyhow!("unknown test name {name}"))?;
        if matching.next().is_some() {
            bail!("multiple executions named {name}; select by execution ID");
        }
        Ok(first)
    }
}

fn either_keys<'a>(a: &'a Value, b: &'a Value) -> BTreeSet<&'a String> {
    a.as_object()
        .into_iter()
        .chain(b.as_object())
        .flat_map(|o| o.keys())
        .collect()
}

fn context(row: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for key in [
        "run_id",
        "execution_id",
        "name",
        "verdict",
        "elapsed_ms",
        "coverage",
        "summary_status",
        "summary_final",
        "closure_incomplete",
        "late_telemetry_observed",
        "late_telemetry_counts",
    ] {
        if let Some(v) = row.get(key) {
            out.insert(key.to_owned(), v.clone());
        }
    }
    if let Some(v) = row.get("summary").and_then(|s| s.get("span_overflow")) {
        out.insert("span_overflow".into(), v.clone());
    }
    out
}

/// Changes are differences between observed counters. Coverage from both
/// source summaries accompanies them; incomplete and sampled-out executions
/// can never establish the absence of a span or attribute.
pub fn compare_summaries(before: &Map<String, Value>, after: &Map<String, Value>) -> Value {
    let mut result = json!({"before":context(before),"after":context(after)});
    let (Some(a), Some(b)) = (
        before.get("summary").filter(|s| s.is_object()),
        after.get("summary").filter(|s| s.is_object()),
    ) else {
        result["comparable"] = false.into();
        return result;
    };
    result["comparable"] = true.into();
    let mut spans = Map::new();
    for key in either_keys(&a["spans"], &b["spans"]) {
        let old = &a["spans"][key];
        let new = &b["spans"][key];
        if old != new {
            spans.insert(key.clone(), json!({"before":old,"after":new,
                "count_change":count_diff(&old["count"], &new["count"]),
                "duration_change_ms":new["total_duration_ms"].as_f64().unwrap_or(0.0)-old["total_duration_ms"].as_f64().unwrap_or(0.0)}));
        }
    }
    result["span_changes"] = spans.into();
    let mut levels = Map::new();
    for key in either_keys(&a["log_levels"], &b["log_levels"]) {
        let (old, new) = (&a["log_levels"][key], &b["log_levels"][key]);
        if old != new {
            levels.insert(
                key.clone(),
                json!({"before":old,"after":new,"count_change":count_diff(old,new)}),
            );
        }
    }
    result["log_level_changes"] = levels.into();
    let mut attrs = Map::new();
    for key in either_keys(&a["distinct_values"], &b["distinct_values"]) {
        let (old, new) = (&a["distinct_values"][key], &b["distinct_values"][key]);
        if old != new {
            attrs.insert(key.clone(), json!({"before":old,"after":new}));
        }
    }
    result["attribute_changes"] = attrs.into();
    result
}

fn count_diff(before: &Value, after: &Value) -> Value {
    let old = before.as_u64().unwrap_or(0);
    let new = after.as_u64().unwrap_or(0);
    if new >= old {
        return (new - old).into();
    }
    // serde_json can represent a negative integer down to i64::MIN.
    Value::from(i64::try_from(i128::from(new) - i128::from(old)).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn final_revision_keeps_the_verdict_case_and_failure_message() {
        let rows=concat!(
            "{\"event\":\"end-test\",\"execution_id\":\"a\",\"message\":\"assertion\",\"verdict\":\"FAIL\",\"summary\":{\"log_levels\":{\"ERROR\":1}}}\n",
            "{\"event\":\"case-ready\",\"execution_id\":\"a\",\"case_path\":\"cases/a.zip\"}\n",
            "{\"event\":\"summary-final\",\"execution_id\":\"a\",\"summary\":{\"log_levels\":{\"ERROR\":2}}}\n",
            "{\"event\":\"run-settled\"}\n",
            "{\"event\":");
        let report = TestLogFile::read(rows.as_bytes()).unwrap();
        let row = report.lookup("a").unwrap();
        assert_eq!(row["message"], "assertion");
        assert_eq!(row["summary"]["log_levels"]["ERROR"], 2);
        assert_eq!(row["case_path"], "cases/a.zip");
        assert!(report.settled && report.partial_last_line);
    }

    #[test]
    fn late_end_cannot_reopen_a_finished_timeout_case() {
        let rows = [
            json!({"event":"case-ready","execution_id":"e","case_status":"ready","case_path":"cases/e.zip"}),
            json!({"event":"end-test","execution_id":"e","case_status":"pending","verdict":"OK"}),
        ].map(|v|v.to_string()).join("\n");
        let report = TestLogFile::read(rows.as_bytes()).unwrap();
        let entry = report.lookup("e").unwrap();
        assert_eq!(entry["case_status"], "ready");
        assert_eq!(entry["verdict"], "OK");
    }

    #[test]
    fn sealed_or_gapped_run_remains_visible_after_settlement() {
        let rows = [
            json!({"event":"begin-test-log","settings":{"summary_sample_percent":25.},"sampling_algorithm":"fnv1a64-v1"}),
            json!({"event":"seal-test-log","incomplete":true}),
            json!({"event":"test-log-gap","lost_records":3}),
            json!({"event":"run-settled"}),
        ].map(|row|row.to_string()).join("\n");
        let summary = TestLogFile::read(rows.as_bytes()).unwrap().journal_status();
        assert_eq!(summary["reported_lost_records"], 3);
        assert_eq!(summary["settled"], true);
        assert_eq!(summary["closed_incomplete"], true);
        assert_eq!(summary["summary_sample_percent"], 25.);
        assert!(TestLogFile::read(br#"{"event":"test-log-gap"}"#.as_slice()).is_err());
    }

    #[test]
    fn comparison_keeps_coverage_and_sampled_out_is_unavailable() {
        let before = json!({"summary":{"spans":{"a":{"count":5,"total_duration_ms":10.0}}},"coverage":{"log":"unknown"}});
        let after = json!({"summary":{"spans":{"a":{"count":2,"total_duration_ms":3.0}}}});
        let changes = compare_summaries(before.as_object().unwrap(), after.as_object().unwrap());
        assert_eq!(changes["span_changes"]["a"]["count_change"], -3);
        assert_eq!(changes["before"]["coverage"]["log"], "unknown");
        let unavailable = compare_summaries(before.as_object().unwrap(), &Map::new());
        assert_eq!(unavailable["comparable"], false);
    }
}
