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
            }
        }
        Ok(report)
    }

    fn reasons(&self, id: &str, row: &Row) -> Vec<&'static str> {
        let mut out = Vec::new();
        if field(row, "verdict") != "OK" {
            out.push("failed or incomplete");
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
            if let Some(original) = self.log.executions.get(field(row, "rerun_of")) {
                let _ = write!(
                    rows,
                    ", rerun of {} ({} ms)",
                    field(row, "rerun_of"),
                    original.get("elapsed_ms").unwrap_or(&Value::Null)
                );
            }
            rows.push('\n');
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
        out.push_str("\n## Exceptional executions\n\nIncludes failed or incomplete executions, reruns, case decisions, deadline crossings, late telemetry and evidence loss. Run-level journal gaps appear in failed-test-log.jsonl.\n\n");
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
