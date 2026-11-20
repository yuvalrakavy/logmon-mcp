//! Offline reporting uses the same latest-revision projection as lookup and compare.
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use logmon_broker_sdk::test_log_file::TestLogFile;
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs::File,
    io::{self, BufRead, BufReader, Write},
    path::{Component, Path, PathBuf},
};
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

type Row = Map<String, Value>;

fn field<'a>(r: &'a Row, k: &str) -> &'a str {
    r.get(k).and_then(Value::as_str).unwrap_or("")
}

pub struct Report {
    log: TestLogFile,
    deadline: BTreeSet<String>,
    samples: logmon_broker_protocol::MachineSeries<Row>,
    loads: Option<(f64, f64)>,
    unresolved: usize,
    zone: Tz,
    settled_at: Option<DateTime<Utc>>,
}

impl Report {
    pub fn open(path: &Path, unresolved: usize) -> Result<Self> {
        let log = TestLogFile::open(path)?;
        let mut report = Self {
            log,
            deadline: BTreeSet::new(),
            samples: Default::default(),
            loads: None,
            unresolved,
            zone: chrono_tz::UTC,
            settled_at: None,
        };
        for line in BufReader::new(File::open(path)?).split(b'\n') {
            let line = line?;
            let row: Row = match serde_json::from_slice(&line) {
                Ok(r) => r,
                Err(_) if report.log.partial_last_line => break,
                Err(e) => return Err(e.into()),
            };
            match field(&row, "event") {
                "begin-test-log" => {
                    if let Some(zone) = row
                        .get("settings")
                        .and_then(|v| v.get("display_timezone"))
                        .and_then(Value::as_str)
                    {
                        report.zone = zone
                            .parse()
                            .with_context(|| format!("invalid report timezone {zone}"))?;
                    }
                }
                "run-settled" => {
                    if let Some(at) = row.get("at") {
                        report.settled_at = Some(
                            serde_json::from_value(at.clone())
                                .context("invalid settlement time")?,
                        );
                    }
                }
                _ => {}
            }
            if field(&row, "case_reason") == "end_test_missing_at_deadline" {
                report.deadline.insert(field(&row, "execution_id").into());
            }
            if field(&row, "event") == "machine-load" {
                if let Some(n) = row
                    .get("fields")
                    .and_then(|v| v.get("load"))
                    .and_then(Value::as_f64)
                {
                    let (lo, hi) = report.loads.get_or_insert((n, n));
                    *lo = lo.min(n);
                    *hi = hi.max(n);
                }
                if let Some(ts) = row
                    .get("ts")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                {
                    let host = field(&row, "host").to_owned();
                    let kind = row
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("machine-load")
                        .to_owned();
                    report.samples.insert(&host, &kind, ts, row);
                }
            }
        }
        Ok(report)
    }

    fn reasons(&self, id: &str, row: &Row) -> Vec<&'static str> {
        let mut out = Vec::new();
        if field(row, "verdict") != "OK" {
            out.push("failed or incomplete");
        }
        if field(row, "fault") == "excessive_telemetry" {
            out.push("excessive_telemetry");
        }
        if !field(row, "rerun_of").is_empty() {
            out.push("rerun");
        }
        if !field(row, "case_status").is_empty() || !field(row, "case_reason").is_empty() {
            out.push("case decision");
        }
        if self.deadline.contains(id) {
            out.push("deadline crossed");
        }
        if row.get("late_telemetry_observed") == Some(&Value::Bool(true)) {
            out.push("late telemetry");
        }
        if row
            .get("coverage")
            .and_then(|c| c.get("domain_loss_during_window"))
            .and_then(Value::as_object)
            .is_some_and(|v| v.values().any(|n| n.as_u64().unwrap_or(0) > 0))
        {
            out.push("telemetry loss during execution");
        }
        if let Some(coverage) = row.get("coverage") {
            for (key, label) in [
                ("log_coverage", "log coverage gap"),
                ("span_coverage", "span coverage gap"),
            ] {
                if matches!(
                    coverage[key]["status"].as_str(),
                    Some("evicted" | "unknown")
                ) {
                    out.push(label);
                }
            }
        }
        out
    }

    pub fn markdown(&self) -> String {
        let mut verdicts = BTreeMap::new();
        let mut reasons = BTreeMap::new();
        let mut slow = Vec::new();
        let mut pending = 0;
        let mut case_errors = 0;
        let mut coverage_gaps = 0;
        let mut telemetry_faults = 0;
        let mut rows = String::new();
        for (id, row) in &self.log.executions {
            let verdict = row
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            *verdicts.entry(verdict).or_insert(0usize) += 1;
            let reason = field(row, "case_reason");
            if !reason.is_empty() {
                *reasons.entry(reason).or_insert(0usize) += 1;
            }
            pending += usize::from(field(row, "case_status") == "pending");
            case_errors += usize::from(field(row, "case_status") == "error");
            let runtime = row
                .get("member_runtime_ms")
                .or_else(|| row.get("fields")?.get("member_runtime_ms"))
                .and_then(Value::as_u64);
            if let (Some(ms), Some(est)) =
                (runtime, row.get("estimated_ms").and_then(Value::as_u64))
            {
                if est > 0 {
                    slow.push((ms as f64 / est as f64, field(row, "name"), ms, est));
                }
            }
            let flags = self.reasons(id, row);
            telemetry_faults += usize::from(flags.contains(&"excessive_telemetry"));
            coverage_gaps += usize::from(flags.iter().any(|s| {
                matches!(
                    *s,
                    "log coverage gap" | "span coverage gap" | "telemetry loss during execution"
                )
            }));
            if flags.is_empty() {
                continue;
            }
            let ms = row
                .get("elapsed_ms")
                .and_then(Value::as_u64)
                .map(|v| format!("{v} ms"))
                .unwrap_or_else(|| "unknown duration".into());
            let _ = write!(
                rows,
                "- {} ({id}) — {verdict}, {ms}, {}",
                field(row, "name"),
                flags.join(", ")
            );
            if !reason.is_empty() {
                let _ = write!(rows, ", {reason}");
            }
            if let Some(state) = row.get("case_status").and_then(Value::as_str) {
                let size = field(row, "case_size");
                if size.is_empty() {
                    let _ = write!(rows, ", case {state}");
                } else {
                    let _ = write!(rows, ", {size} case {state}");
                }
                let detail = field(row, "case_detail");
                if !detail.is_empty() {
                    let _ = write!(rows, " ({detail})");
                }
            }
            if let Some(path) = row.get("case_path").and_then(Value::as_str) {
                let _ = write!(rows, " [case](<{path}>)");
            }
            if field(row, "fault") == "excessive_telemetry" {
                append_telemetry_details(&mut rows, row);
            }
            if let Some(original) = self.log.executions.get(field(row, "rerun_of")) {
                let _ = write!(
                    rows,
                    ", rerun of {} ({} ms)",
                    field(row, "rerun_of"),
                    original.get("elapsed_ms").unwrap_or(&Value::Null)
                );
            }
            rows.push('\n');
            if verdict != "OK" || reason == "slow_pass" || self.deadline.contains(id) {
                let time = |key: &str| {
                    row.get(key)
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                };
                if let (Some(start), Some(end)) = (
                    time("started_at"),
                    time("ended_at")
                        .or_else(|| time("captured_at"))
                        .or_else(|| {
                            row.get("coverage")?
                                .get("case_snapshot_at")
                                .and_then(|v| serde_json::from_value(v.clone()).ok())
                        }),
                ) {
                    for sample in self.samples.window(start, end, false) {
                        let _ = writeln!(
                            rows,
                            "      {} {} {} {}",
                            field(sample, "ts"),
                            field(sample, "host").escape_debug(),
                            sample
                                .get("kind")
                                .and_then(Value::as_str)
                                .unwrap_or("machine-load")
                                .escape_debug(),
                            logmon_broker_protocol::machine_fields(
                                sample.get("fields").unwrap_or(&Value::Null)
                            )
                        );
                    }
                }
            }
        }
        let status = if !self.log.settled {
            "Finalization pending"
        } else if self.log.sealed {
            "Recovered after explicit seal"
        } else if self.log.closed_incomplete
            || self.log.reported_lost_records > 0
            || self.log.partial_last_line
            || self.unresolved > 0
            || pending > 0
            || case_errors > 0
            || coverage_gaps > 0
        {
            "Completed with evidence gaps"
        } else if telemetry_faults > 0 {
            "Completed with telemetry limit faults"
        } else {
            "Test-log completed normally"
        };
        let mut out = format!("# {}\n\n{}\n\n{} executions. Lost journal records {}. Unresolved ends {}. Pending cases {}. Case write errors {}.\n",self.log.run_id.as_deref().unwrap_or("Test log"),status,self.log.executions.len(),self.log.reported_lost_records,self.unresolved,pending,case_errors);
        if coverage_gaps > 0 {
            let _ = writeln!(
                out,
                "Executions with known loss or incomplete ring coverage: {coverage_gaps}."
            );
        }
        if telemetry_faults > 0 {
            let _ = writeln!(
                out,
                "Executions with telemetry limit faults: {telemetry_faults}."
            );
        }
        if let Some(at) = self.settled_at {
            let _ = writeln!(
                out,
                "Settled at {} ({}).",
                at.with_timezone(&self.zone).to_rfc3339(),
                self.zone
            );
        }
        for (v, n) in verdicts {
            let _ = writeln!(out, "- {v}: {n}");
        }
        for (v, n) in reasons {
            let _ = writeln!(out, "- Case trigger {v}: {n}");
        }
        if let Some((lo, hi)) = self.loads {
            let _ = writeln!(out, "\nSampled machine load: {lo}–{hi}.");
        }
        out.push_str("\n## Exceptional executions\n\nIncludes failed or incomplete executions, reruns, case decisions, deadline crossings, telemetry limit faults, late telemetry and evidence loss. Run-level journal gaps appear in failed-test-log.jsonl.\n\n");
        out.push_str(if rows.is_empty() { "None.\n" } else { &rows });
        slow.sort_by(|a, b| b.0.total_cmp(&a.0));
        if !slow.is_empty() {
            out.push_str("\n## Slowest member runtimes against estimates\n\n");
            for (ratio, name, ms, est) in slow.into_iter().take(10) {
                let _ = writeln!(out, "- {name}: {ms} ms / {est} ms ({ratio:.2}×)");
            }
        }
        out
    }
}

fn append_telemetry_details(out: &mut String, row: &Row) {
    let flatten = logmon_broker_core::render::escape::flatten;
    if let Some(refused) = row.get("refused_telemetry") {
        let logs = refused["logs"].as_u64().unwrap_or(0);
        let spans = refused["spans"].as_u64().unwrap_or(0);
        let _ = write!(out, ", refused telemetry (logs {logs}, spans {spans})");
    }
    if let Some(limit) = row.get("telemetry_limit") {
        if let (Some(kind), Some(count), Some(ring)) = (
            limit["kind"].as_str(),
            limit["limit"].as_u64(),
            limit["ring_size"].as_u64(),
        ) {
            let _ = write!(
                out,
                ", limit {count} {} in a {ring}-record ring",
                flatten(kind)
            );
        }
        if let Some(peers) = limit["shared_with"].as_array() {
            for peer in peers {
                if let Some(name) = peer["name"].as_str() {
                    let _ = write!(out, ", shares the selector of {}", flatten(name));
                }
            }
        }
    }
    if let Some(path) = row.get("telemetry_tail_path").and_then(Value::as_str) {
        let _ = write!(out, " [telemetry tail](<{path}>)");
    }
    if let Some(shortened) = row.get("telemetry_tail_shortened") {
        let logs = shortened["logs"].as_u64().unwrap_or(0);
        let spans = shortened["spans"].as_u64().unwrap_or(0);
        if logs > 0 || spans > 0 {
            let _ = write!(out, ", tail shortened (logs {logs}, spans {spans})");
        }
    }
    if let Some(error) = row.get("telemetry_tail_error").and_then(Value::as_str) {
        let _ = write!(out, ", telemetry tail write error ({})", flatten(error));
    }
}

fn copy_member<W: Write + io::Seek>(
    out: &mut ZipWriter<W>,
    name: &str,
    mut src: impl io::Read,
) -> Result<()> {
    out.start_file(
        name,
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
    )?;
    io::copy(&mut src, out)?;
    Ok(())
}

fn case_member(name: &str, stem: &str) -> bool {
    ["md", "logdata.jsonl", "spandata.jsonl", "registry.json"]
        .iter()
        .any(|ext| name == format!("{stem}.{ext}"))
}

fn append_case<W: Write + io::Seek>(
    out: &mut ZipWriter<W>,
    root: &Path,
    reference: &str,
) -> Result<String> {
    let rel = Path::new(reference);
    ensure!(
        rel.components().all(|c| matches!(c, Component::Normal(_))) && rel.starts_with("cases"),
        "invalid case reference {reference}"
    );
    let source = root
        .join(rel)
        .canonicalize()
        .with_context(|| format!("opening case {reference}"))?;
    ensure!(source.starts_with(root), "case outside journal directory");
    let name = rel
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid case filename")?;
    if let Some(stem) = name.strip_suffix(".case.zip") {
        let mut archive = ZipArchive::new(File::open(source)?)?;
        let mut copied = BTreeSet::new();
        for idx in 0..archive.len() {
            let entry = archive.by_index(idx)?;
            let member = entry.name().to_owned();
            ensure!(
                case_member(&member, stem) && copied.insert(member.clone()),
                "unexpected or duplicate case member {member}"
            );
            copy_member(out, &format!("cases/{stem}/{member}"), entry)?;
        }
        ensure!(copied.contains(&format!("{stem}.md")), "case has no report");
        return Ok(format!("cases/{stem}/{stem}.md"));
    }
    let stem = name
        .strip_suffix(".md")
        .context("case reference must name a case ZIP or Markdown report")?;
    ensure!(
        rel == PathBuf::from("cases").join(stem).join(name),
        "invalid expanded case layout {reference}"
    );
    let dir = source.parent().context("missing case directory")?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let member = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid case member"))?;
        ensure!(
            entry.file_type()?.is_file(),
            "unexpected case member {member}"
        );
        ensure!(
            case_member(&member, stem),
            "unsupported case member {member}"
        );
        copy_member(
            out,
            &format!("cases/{stem}/{member}"),
            File::open(entry.path())?,
        )?;
    }
    Ok(reference.into())
}

fn append_telemetry_tail<W: Write + io::Seek>(
    out: &mut ZipWriter<W>,
    root: &Path,
    reference: &str,
) -> Result<()> {
    let rel = Path::new(reference);
    ensure!(
        rel.parent() == Some(Path::new("cases"))
            && rel
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            && reference.ends_with(".telemetry-tail.jsonl"),
        "invalid telemetry tail reference {reference}"
    );
    let source = root
        .join(rel)
        .canonicalize()
        .with_context(|| format!("opening telemetry tail {reference}"))?;
    ensure!(
        source.starts_with(root.join("cases")),
        "telemetry tail outside cases directory"
    );
    copy_member(out, reference, File::open(source)?)
}

pub fn archive(path: &Path, output: &Path, unresolved: usize) -> Result<()> {
    let path = path.canonicalize()?;
    let root = path.parent().context("missing journal directory")?;
    let mut report = Report::open(&path, unresolved)?;
    ensure!(
        report.log.settled && !report.log.partial_last_line,
        "journal is provisional; use summary for current results"
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(
        output.canonicalize().ok().as_ref() != Some(&path),
        "output would replace journal"
    );
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    let mut out = ZipWriter::new(tmp.as_file_mut());
    out.add_directory("cases/", SimpleFileOptions::default())?;
    let mut refs = BTreeMap::new();
    let mut tails = BTreeSet::new();
    for row in report.log.executions.values_mut() {
        if let Some(Value::String(reference)) = row.get_mut("case_path") {
            let archived = match refs.get(reference) {
                Some(value) => value,
                None => {
                    let new_ref = append_case(&mut out, root, reference)?;
                    refs.entry(reference.clone()).or_insert(new_ref)
                }
            };
            reference.clone_from(archived);
        }
        let tail = field(row, "telemetry_tail_path");
        if !tail.is_empty() && tails.insert(tail.to_owned()) {
            append_telemetry_tail(&mut out, root, tail)?;
        }
    }
    let exceptional: BTreeSet<_> = report
        .log
        .executions
        .iter()
        .filter(|(id, row)| !report.reasons(id, row).is_empty())
        .map(|(id, _)| id.as_str())
        .collect();
    let mut failed = tempfile::tempfile()?;
    out.start_file(
        "full-test-log.jsonl",
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
    )?;
    for line in BufReader::new(File::open(&path)?).split(b'\n') {
        let line = line?;
        let mut row: Row = serde_json::from_slice(&line)?;
        let include = exceptional.contains(field(&row, "execution_id"))
            || field(&row, "event") == "test-log-gap";
        let bytes = if let Some(Value::String(reference)) = row.get_mut("case_path") {
            let dst = refs
                .get(reference)
                .context("case path has no final disposition")?;
            if dst != reference {
                reference.clone_from(dst);
                serde_json::to_vec(&row)?
            } else {
                line
            }
        } else {
            line
        };
        out.write_all(&bytes)?;
        out.write_all(b"\n")?;
        if include {
            failed.write_all(&bytes)?;
            failed.write_all(b"\n")?;
        }
    }
    io::Seek::rewind(&mut failed)?;
    copy_member(&mut out, "failed-test-log.jsonl", failed)?;
    copy_member(&mut out, "summary.md", report.markdown().as_bytes())?;
    out.finish()?;
    tmp.as_file().sync_all()?;
    tmp.persist(output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn write_rows(path: &Path, rows: &[serde_json::Value]) {
        let mut file = File::create(path).unwrap();
        for row in rows {
            writeln!(file, "{row}").unwrap();
        }
    }

    #[test]
    fn report_machine_series_scopes_slow_and_failed_executions() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("series.jsonl");
        let ts = "2026-11-20T00:01:00Z";
        let mut rows = vec![
            json!({"event":"machine-load","host":"a","ts":ts,"fields":{"load":4,"idle":70,"abnormal":"starved"}}),
            json!({"event":"machine-load","host":"a","ts":"2026-11-20T00:00:00Z","kind":"recorder","fields":{"outside":true}}),
        ];
        for (id, verdict, reason) in [
            ("a", "FAIL", "failure"),
            ("b", "OK", "slow_pass"),
            ("c", "OK", ""),
        ] {
            rows.push(json!({"event":"end-test","execution_id":id,"started_at":ts,"ended_at":ts,"verdict":verdict,"case_reason":reason}));
        }
        write_rows(&journal, &rows);
        let md = Report::open(&journal, 0).unwrap().markdown();
        assert_eq!(
            md.matches("machine-load load=4 idle=70 abnormal=").count(),
            2,
            "{md}"
        );
        assert!(!md.contains("outside=true"));
    }

    #[test]
    fn healthy_archive_and_pending_replacement() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("test.jsonl");
        let output = dir.path().join("run.zip");
        let rows = [
            json!({"event":"begin-test-log","run_id":"green"}),
            json!({"event":"end-test","execution_id":"a","name":"example","verdict":"OK"}),
            json!({"event":"run-settled"}),
        ];
        write_rows(&journal, &rows);
        archive(&journal, &output, 0).unwrap();
        let before = std::fs::read(&output).unwrap();
        let mut zip = ZipArchive::new(io::Cursor::new(&before)).unwrap();
        assert_eq!(zip.by_name("failed-test-log.jsonl").unwrap().size(), 0);
        assert!(zip.by_name("cases/").unwrap().is_dir());
        let mut summary = String::new();
        zip.by_name("summary.md")
            .unwrap()
            .read_to_string(&mut summary)
            .unwrap();
        assert!(summary.contains("Test-log completed normally"));
        write_rows(&journal, &rows[..2]);
        assert!(archive(&journal, &output, 0).is_err());
        assert_eq!(std::fs::read(output).unwrap(), before);
        assert!(Report::open(&journal, 0)
            .unwrap()
            .markdown()
            .contains("Finalization pending"));
    }

    #[test]
    fn reader_displaced_slow_pass_renders_a_skip_without_a_case_size() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("test.jsonl");
        write_rows(
            &journal,
            &[
                json!({"event":"end-test","execution_id":"a","name":"slow test","verdict":"OK",
                    "case_status":"pending","case_size":"full"}),
                json!({"event":"case-skipped","execution_id":"a","case_status":"evicted_for_priority"}),
                json!({"event":"run-settled"}),
            ],
        );
        let report = Report::open(&journal, 0).unwrap().markdown();
        assert!(
            !report.contains("full case evicted_for_priority"),
            "{report}"
        );
        assert!(report.contains("case evicted_for_priority"), "{report}");
    }

    #[test]
    fn b1_report_and_archive_keep_an_ok_telemetry_fault_and_its_tail() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("test.jsonl");
        let output = dir.path().join("run.zip");
        let tail = "cases/a.telemetry-tail.jsonl";
        std::fs::create_dir(dir.path().join("cases")).unwrap();
        std::fs::write(dir.path().join(tail), "{\"record\":{}}\n").unwrap();
        write_rows(
            &journal,
            &[
                json!({"event":"end-test","execution_id":"a","name":"test a","verdict":"OK",
                "fault":"excessive_telemetry","refused_telemetry":{"logs":2,"spans":1},
                "telemetry_tail_shortened":{"logs":1,"spans":0},
                "telemetry_limit":{"kind":"logs","limit":5,"ring_size":20,
                    "shared_with":[{"execution_id":"b","name":"test b"}]},
                "telemetry_tail_path":tail}),
                json!({"event":"run-settled"}),
            ],
        );
        let summary = Report::open(&journal, 0).unwrap().markdown();
        assert!(summary.contains("test a (a) — OK"), "{summary}");
        assert!(summary.contains("excessive_telemetry"));
        assert!(summary.contains("refused telemetry (logs 2, spans 1)"));
        assert!(summary.contains("tail shortened (logs 1, spans 0)"));
        assert!(summary.contains("5 logs in a 20-record ring"));
        assert!(summary.contains("shares the selector of test b"));
        assert!(summary.contains(tail));
        archive(&journal, &output, 0).unwrap();
        let mut zip = ZipArchive::new(File::open(output).unwrap()).unwrap();
        assert!(zip.by_name(tail).is_ok());
        let mut failed = String::new();
        zip.by_name("failed-test-log.jsonl")
            .unwrap()
            .read_to_string(&mut failed)
            .unwrap();
        assert!(failed.contains("excessive_telemetry"));
    }

    #[test]
    fn shared_loose_case_and_journal_gap_round_trip() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("test.jsonl");
        let output = dir.path().join("run.zip");
        let folder = dir.path().join("cases/group");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("group.md"), "case").unwrap();
        let rows = [
            json!({"event":"end-test","execution_id":"a","verdict":"FAIL"}),
            json!({"event":"case-ready","execution_id":"a","case_status":"ready","case_path":"cases/group/group.md"}),
            json!({"event":"end-test","execution_id":"b","verdict":"OK","rerun_of":"a"}),
            json!({"event":"case-ready","execution_id":"b","case_status":"ready","case_path":"cases/group/group.md"}),
            json!({"event":"test-log-gap","lost_records":2}),
            json!({"event":"run-settled"}),
        ];
        write_rows(&journal, &rows);
        archive(&journal, &output, 1).unwrap();
        let mut zip = ZipArchive::new(File::open(output).unwrap()).unwrap();
        assert_eq!(
            zip.file_names()
                .filter(|s| *s == "cases/group/group.md")
                .count(),
            1
        );
        let mut failed = String::new();
        zip.by_name("failed-test-log.jsonl")
            .unwrap()
            .read_to_string(&mut failed)
            .unwrap();
        assert_eq!(failed.lines().count(), 5);
        let mut summary = String::new();
        zip.by_name("summary.md")
            .unwrap()
            .read_to_string(&mut summary)
            .unwrap();
        assert!(summary.contains("Completed with evidence gaps"));
    }
}
