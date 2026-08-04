//! Claiming and filling a case's three files — spec §5.2's bulk half and
//! §5.3's collision rule.
//!
//! **The daemon writes, which is why `dir` is a parameter.** Returning ~400 KB
//! of JSONL as an RPC result would put the whole archive into the model's
//! context, which is exactly what §5.2's document/logdata split exists to
//! prevent.
//!
//! **Create-new-exclusive, then re-roll — not check-then-write.** Between a
//! check and a write another process can land, and in an archive nothing ever
//! deletes from, the loser's evidence is gone with no error anywhere. The
//! filesystem's own exclusivity is the only check that does not race.

use super::naming;
use super::FORMAT_VERSION;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Re-rolls before the call fails. Each attempt draws four fresh hex
/// characters, so exhausting this means something other than collision.
pub const MAX_STEM_ATTEMPTS: usize = 8;

/// The three files of one case, claimed exclusively and open for writing.
pub struct CaseFiles {
    pub stem: String,
    /// Document, logdata, spandata — the order they are reported in.
    pub paths: [PathBuf; 3],
    pub document: File,
    pub logdata: File,
    pub spandata: File,
}

/// What a written file came to — reported on the wire so a caller never has to
/// stat the archive to find out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaseFileStats {
    /// Data records, **excluding** the format header.
    pub records: u64,
    /// Total file size, **including** the header — what `ls` will show.
    pub bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum CaseWriteError {
    #[error("could not prepare directory {0}: {1}")]
    Directory(PathBuf, io::Error),
    #[error("could not claim a free filename in {0} after {1} attempts")]
    Exhausted(PathBuf, usize),
    #[error("could not create {0}: {1}")]
    Create(PathBuf, io::Error),
    #[error("could not write {0}: {1}")]
    Write(PathBuf, io::Error),
}

/// Claim `<prefix>-<yymmdd>-<hhmmss>[-<id>]` and open all three files.
///
/// The directory is created if it is missing: an archive directory is expected
/// to come into being on the first capture, and losing the evidence because it
/// did not exist yet is the worse failure. `dir` is required to be absolute by
/// the caller — a relative path would resolve against the daemon's working
/// directory rather than the caller's — so this cannot walk somewhere
/// surprising from a bare name.
pub fn claim(dir: &Path, prefix: &str, at: DateTime<Utc>) -> Result<CaseFiles, CaseWriteError> {
    fs::create_dir_all(dir).map_err(|e| CaseWriteError::Directory(dir.to_path_buf(), e))?;
    let base = naming::stem(prefix, at);
    for attempt in 0..MAX_STEM_ATTEMPTS {
        // The first attempt carries no id, so an uncontended archive reads as
        // `checkout-hang-260731-021530.md` rather than as a hash.
        let stem = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}-{}", naming::roll_id())
        };
        match try_claim(dir, &stem) {
            Ok(files) => return Ok(files),
            Err(ClaimFailure::Taken) => continue,
            Err(ClaimFailure::Io(path, e)) => return Err(CaseWriteError::Create(path, e)),
        }
    }
    Err(CaseWriteError::Exhausted(
        dir.to_path_buf(),
        MAX_STEM_ATTEMPTS,
    ))
}

/// A claimed bundle: one file, exclusively created.
pub struct CaseBundleFile {
    pub stem: String,
    pub path: PathBuf,
    pub file: File,
}

/// Claim `<stem>.case.zip`, re-rolling the stem on collision.
///
/// **Same create-new-exclusive discipline as [`claim`], and for the same
/// reason**: between a check and a write another process can land, and in an
/// archive nothing ever deletes from, the loser's evidence is gone with no error
/// anywhere. One file instead of three makes the claim simpler, not weaker — a
/// bundle cannot be half-claimed, which is the failure `try_claim` exists to
/// prevent for the loose form.
pub fn claim_bundle(
    dir: &Path,
    prefix: &str,
    at: DateTime<Utc>,
) -> Result<CaseBundleFile, CaseWriteError> {
    fs::create_dir_all(dir).map_err(|e| CaseWriteError::Directory(dir.to_path_buf(), e))?;
    let base = naming::stem(prefix, at);
    for attempt in 0..MAX_STEM_ATTEMPTS {
        let stem = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}-{}", naming::roll_id())
        };
        let path = dir.join(super::bundle::bundle_name(&stem));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok(CaseBundleFile { stem, path, file }),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(CaseWriteError::Create(path, e)),
        }
    }
    Err(CaseWriteError::Exhausted(
        dir.to_path_buf(),
        MAX_STEM_ATTEMPTS,
    ))
}

enum ClaimFailure {
    Taken,
    Io(PathBuf, io::Error),
}

/// All three or none. A stem whose `.md` is free but whose `.logdata.jsonl` is
/// not has been half-claimed by something, and taking it anyway would write the
/// document's own pointers at a file another writer owns.
fn try_claim(dir: &Path, stem: &str) -> Result<CaseFiles, ClaimFailure> {
    let names = naming::file_names(stem);
    let paths: [PathBuf; 3] = std::array::from_fn(|i| dir.join(&names[i]));

    let mut opened: Vec<File> = Vec::with_capacity(3);
    for (i, path) in paths.iter().enumerate() {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(f) => opened.push(f),
            Err(e) => {
                // Undo the partial claim. Every path being removed here was
                // created by THIS call microseconds ago under `create_new`, so
                // ownership is not in question — and leaving them would strand
                // an empty file under a stem the re-roll is about to abandon.
                drop(opened);
                for stranded in &paths[..i] {
                    let _ = fs::remove_file(stranded);
                }
                return Err(if e.kind() == io::ErrorKind::AlreadyExists {
                    ClaimFailure::Taken
                } else {
                    ClaimFailure::Io(path.clone(), e)
                });
            }
        }
    }

    let mut opened = opened.into_iter();
    Ok(CaseFiles {
        stem: stem.to_string(),
        paths,
        document: opened.next().expect("three opened"),
        logdata: opened.next().expect("three opened"),
        spandata: opened.next().expect("three opened"),
    })
}

/// The first line of every JSONL file in the archive.
///
/// A struct rather than a `json!` literal so the field order is the declaration
/// order — `serde_json`'s maps sort their keys, and a header a reader will see
/// verbatim for years should read the way it was designed to.
#[derive(Serialize)]
struct JsonlHeader<'a> {
    logmon_format: u32,
    kind: &'a str,
}

/// Write one JSONL file: a format header, then one record per line.
///
/// **The header is the contract.** §2 rules out any query engine over the
/// archive, so the format *is* what a future reader has — add a field to
/// `LogEntry` in 0.12 and an unversioned archive holds two record shapes with
/// nothing distinguishing them. `/logmon/version` in the registry copy is a
/// proxy, not a contract; §3.7 shows it can be stale.
///
/// A file with no records is still written, with its header: absent cannot be
/// told from "we captured none".
/// Generic over the sink so a bundle can assemble evidence in memory before
/// compressing it. `path` stays for the error message: a caller writing to a
/// `Vec` still names the entry the bytes were destined for, or a failure reads
/// as coming from nowhere.
pub fn write_jsonl<W: Write, T: Serialize>(
    sink: W,
    path: &Path,
    kind: &str,
    records: impl IntoIterator<Item = T>,
) -> Result<CaseFileStats, CaseWriteError> {
    let fail = |e: io::Error| CaseWriteError::Write(path.to_path_buf(), e);
    let mut out = BufWriter::new(sink);
    let mut stats = CaseFileStats::default();

    let header = JsonlHeader {
        logmon_format: FORMAT_VERSION,
        kind,
    };
    let mut line = serde_json::to_string(&header).map_err(|e| fail(e.into()))?;
    line.push('\n');
    out.write_all(line.as_bytes()).map_err(fail)?;
    stats.bytes += line.len() as u64;

    for record in records {
        let mut line = serde_json::to_string(&record).map_err(|e| fail(e.into()))?;
        line.push('\n');
        out.write_all(line.as_bytes()).map_err(fail)?;
        stats.bytes += line.len() as u64;
        stats.records += 1;
    }
    out.flush().map_err(fail)?;
    Ok(stats)
}

/// Write the document body and report its size.
///
/// Measured because the document is the artifact whose size can run away: §3.1
/// permits 1.1 MB of registry and the document renders it, so the one file that
/// could be pathological is the one that gets a number.
pub fn write_document(file: File, path: &Path, body: &str) -> Result<u64, CaseWriteError> {
    let fail = |e: io::Error| CaseWriteError::Write(path.to_path_buf(), e);
    let mut out = BufWriter::new(file);
    out.write_all(body.as_bytes()).map_err(fail)?;
    out.flush().map_err(fail)?;
    Ok(body.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-31T02:15:30Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn a_claim_opens_three_files_sharing_one_stem() {
        let dir = tempfile::tempdir().unwrap();
        let files = claim(dir.path(), "checkout-hang", at()).unwrap();
        assert_eq!(files.stem, "checkout-hang-260731-021530");
        for p in &files.paths {
            assert!(p.exists(), "{p:?}");
            assert!(p.starts_with(dir.path()));
        }
    }

    #[test]
    fn the_directory_is_created_rather_than_the_capture_lost() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("docs").join("cases");
        let files = claim(&nested, "x", at()).unwrap();
        assert!(files.paths[0].exists());
    }

    #[test]
    fn two_captures_in_the_same_second_produce_two_sets_of_files() {
        // Driven CONCURRENTLY, because `create_new` is what makes this true and
        // a sequential test passes against a check-then-write that races.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || claim(&path, "checkout-hang", at()).unwrap().stem)
            })
            .collect();
        let stems: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let distinct: HashSet<&String> = stems.iter().collect();
        assert_eq!(
            distinct.len(),
            8,
            "eight captures in one second must not overwrite each other: {stems:?}"
        );
        // Exactly one is the bare stem; the rest carry a rolled id.
        assert_eq!(
            stems
                .iter()
                .filter(|s| *s == "checkout-hang-260731-021530")
                .count(),
            1,
            "{stems:?}"
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            24,
            "three files per capture, none clobbered"
        );
    }

    #[test]
    fn a_half_claimed_stem_is_abandoned_whole() {
        let dir = tempfile::tempdir().unwrap();
        // Someone else already owns the logdata under the bare stem — an
        // aborted earlier run, say. The document is free, and taking it would
        // point the case at a file this call does not own.
        let squatted = dir.path().join("x-260731-021530.logdata.jsonl");
        std::fs::write(&squatted, b"not ours").unwrap();

        let files = claim(dir.path(), "x", at()).unwrap();
        assert_ne!(files.stem, "x-260731-021530");
        assert!(
            !dir.path().join("x-260731-021530.md").exists(),
            "the partially-claimed document must not be stranded"
        );
        assert_eq!(
            std::fs::read_to_string(&squatted).unwrap(),
            "not ours",
            "and the squatter's file is untouched"
        );
    }

    #[test]
    fn every_jsonl_file_opens_with_its_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let files = claim(dir.path(), "x", at()).unwrap();

        let stats = write_jsonl(
            files.logdata,
            &files.paths[1],
            "logdata",
            vec![serde_json::json!({"seq": 1}), serde_json::json!({"seq": 2})],
        )
        .unwrap();
        assert_eq!(stats.records, 2, "the header is not a record");

        let text = std::fs::read_to_string(&files.paths[1]).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next().unwrap(),
            r#"{"logmon_format":1,"kind":"logdata"}"#
        );
        assert_eq!(lines.next().unwrap(), r#"{"seq":1}"#);
        assert_eq!(stats.bytes, text.len() as u64, "bytes is the file's size");
    }

    #[test]
    fn a_file_with_no_records_is_still_written() {
        // Absent cannot be told from "we captured none".
        let dir = tempfile::tempdir().unwrap();
        let files = claim(dir.path(), "x", at()).unwrap();
        let stats = write_jsonl(
            files.spandata,
            &files.paths[2],
            "spandata",
            Vec::<serde_json::Value>::new(),
        )
        .unwrap();
        assert_eq!(stats.records, 0);
        assert!(files.paths[2].exists());
        assert!(stats.bytes > 0, "the header is still there");
    }
}
