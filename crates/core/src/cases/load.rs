//! Reading a case off disk into the shape a postmortem domain is built from.
//!
//! **Everything here is validation.** The bytes arrived from another machine, by
//! `scp` or an artifact store or an email attachment, and the daemon reads them
//! — which is why `create_case`'s own rationale (keep the archive out of the
//! model's context) puts the loader daemon-side, and why it must not be
//! trusting. A case that cannot be validated is refused; it is never partially
//! loaded, because a domain that silently holds half a case answers questions
//! about a system with evidence nobody chose.

use std::path::{Path, PathBuf};

use crate::gelf::message::LogEntry;
use crate::span::types::SpanEntry;

use super::bundle::{self, BundleError, CaseBytes};
use super::read::{self, FrontMatter, ParseError, RegistryBlock};

/// Hard ceiling on records, mirroring `domains.create`'s buffer cap.
///
/// **`logdata.records` is a number in a text file.** Sizing a ring from it
/// reaches `reserve_exact` unchecked: `18446744073709551615` panics, and
/// `200000000` asks for tens of gigabytes and aborts the process, taking every
/// live domain's buffers with it. The front-matter count is a CROSS-CHECK, never
/// an allocation size — capacity comes from records actually parsed.
pub const MAX_CASE_RECORDS: usize = 10_000_000;

#[derive(Debug)]
pub enum LoadError {
    NotAbsolute(PathBuf),
    Io(PathBuf, std::io::Error),
    Parse(ParseError),
    Bundle(BundleError),
    /// A front-matter pointer that does not name this case's own file.
    ForeignPointer { key: &'static str, file: String },
    /// The front matter declares evidence the archive does not hold.
    DeclaredButMissing(String),
    /// A JSONL header that is absent, wrong-kind, or newer than this build.
    BadHeader(String),
    /// Records that the stores cannot hold as given.
    BadRecords(String),
    /// The count the front matter declares is not the count that parsed.
    CountMismatch {
        key: &'static str,
        declared: u64,
        parsed: usize,
    },
    TooManyRecords(usize),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAbsolute(p) => write!(
                f,
                "`{}` is not an absolute path. The broker runs as a service, so a \
                 relative path would resolve against its working directory rather \
                 than yours",
                p.display()
            ),
            Self::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Self::Parse(e) => write!(f, "{e}"),
            Self::Bundle(e) => write!(f, "{e}"),
            Self::ForeignPointer { key, file } => write!(
                f,
                "the front matter's `{key}` names `{file}`, which does not belong \
                 to this case. A case's evidence files are named after its own \
                 stem, and a pointer that is not is either a corrupted document \
                 or an attempt to make the daemon read something else"
            ),
            Self::DeclaredButMissing(n) => write!(
                f,
                "the front matter declares `{n}` but it is not there. That is \
                 corruption, not omission — an omitted file has no front-matter \
                 key at all"
            ),
            Self::BadHeader(m) => write!(f, "{m}"),
            Self::BadRecords(m) => write!(f, "{m}"),
            Self::CountMismatch {
                key,
                declared,
                parsed,
            } => write!(
                f,
                "`{key}` declares {declared} record(s) and {parsed} parsed. The \
                 file is truncated or the document does not belong to it"
            ),
            Self::TooManyRecords(n) => write!(
                f,
                "this case holds {n} records, above the {MAX_CASE_RECORDS} ceiling \
                 a single domain may hold"
            ),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<ParseError> for LoadError {
    fn from(e: ParseError) -> Self {
        Self::Parse(e)
    }
}
impl From<BundleError> for LoadError {
    fn from(e: BundleError) -> Self {
        Self::Bundle(e)
    }
}

/// A case, validated and ready to become a domain.
#[derive(Debug)]
pub struct LoadedCase {
    pub stem: String,
    pub front: FrontMatter,
    /// `None` when the capture omitted this evidence — distinct from
    /// `Some(vec![])`, which is "we captured none".
    pub logs: Option<Vec<LogEntry>>,
    pub spans: Option<Vec<SpanEntry>>,
    pub registry: Option<RegistryBlock>,
}

impl LoadedCase {
    /// The window's lower bound, which is also `lost_below` and one above the
    /// epoch origin.
    pub fn from_seq(&self) -> u64 {
        self.front.seq_range.from
    }
    pub fn to_seq(&self) -> u64 {
        self.front.seq_range.to
    }
}

/// Read a case from an absolute path — a `.case.zip` or a case document.
pub fn load(path: &Path) -> Result<LoadedCase, LoadError> {
    if !path.is_absolute() {
        return Err(LoadError::NotAbsolute(path.to_path_buf()));
    }

    let bytes = match bundle::stem_of_bundle(path) {
        Some(stem) => {
            let f = std::fs::File::open(path).map_err(|e| LoadError::Io(path.into(), e))?;
            bundle::unpack(f, &stem)?
        }
        None => read_loose(path)?,
    };

    // **Not lossy.** `from_utf8_lossy` turns a flipped byte into U+FFFD and the
    // document then parses cleanly — a case that loads "successfully" with
    // silently wrong evidence, against this module's own promise that a case is
    // never partially loaded. The bundle form has per-entry CRCs and would catch
    // it first; the LOOSE form has no checksum at all, and `scp` is the design's
    // own transport.
    let doc = String::from_utf8(bytes.document.clone())
        .map_err(|e| LoadError::BadRecords(format!("the case document is not valid UTF-8: {e}")))?;
    let front = read::parse_front_matter(&doc)?;

    // The pointers are followed by the DAEMON, so they are checked before
    // anything is opened. An allowlist against the case's own stem, not a
    // traversal blocklist — see `bundle::permitted` for the same reasoning.
    let [_, want_log, want_span] = super::naming::file_names(&bytes.stem);
    check_pointer("logdata", front.logdata.as_ref().map(|r| &r.file), &want_log)?;
    check_pointer(
        "spandata",
        front.spandata.as_ref().map(|r| &r.file),
        &want_span,
    )?;

    let logs = evidence("logdata", &front, &bytes.logdata, "logdata")?;
    let spans = evidence("spandata", &front, &bytes.spandata, "spandata")?;

    let total = logs.as_ref().map_or(0, Vec::len) + spans.as_ref().map_or(0, Vec::len);
    if total > MAX_CASE_RECORDS {
        return Err(LoadError::TooManyRecords(total));
    }

    validate_seqs("logdata", logs.as_deref(), &front)?;
    validate_seqs("spandata", spans.as_deref(), &front)?;

    let registry = read::parse_registry(bytes.registry.as_deref())?;

    Ok(LoadedCase {
        stem: bytes.stem,
        front,
        logs,
        spans,
        registry,
    })
}

/// A case as loose files: the document, plus whichever siblings exist.
fn read_loose(md: &Path) -> Result<CaseBytes, LoadError> {
    let stem = md
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| LoadError::NotAbsolute(md.to_path_buf()))?
        .to_string();
    let dir = md.parent().unwrap_or(Path::new("/"));
    let [_, logdata, spandata] = super::naming::file_names(&stem);

    let read_opt = |name: &str| -> Result<Option<Vec<u8>>, LoadError> {
        let p = dir.join(name);
        match std::fs::read(&p) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(LoadError::Io(p, e)),
        }
    };

    Ok(CaseBytes {
        document: std::fs::read(md).map_err(|e| LoadError::Io(md.into(), e))?,
        logdata: read_opt(&logdata)?,
        spandata: read_opt(&spandata)?,
        registry: read_opt(&bundle::registry_entry_name(&stem))?,
        stem,
    })
}

fn check_pointer(
    key: &'static str,
    declared: Option<&String>,
    want: &str,
) -> Result<(), LoadError> {
    match declared {
        None => Ok(()),
        Some(f) if f == want => Ok(()),
        Some(f) => Err(LoadError::ForeignPointer {
            key,
            file: f.clone(),
        }),
    }
}

/// Parse one evidence file, checking its header and its declared count.
///
/// The three-way rule lives here: a declared key with bytes is evidence, an
/// absent key with no bytes is omission, and a declared key with no bytes is
/// corruption. Silence on the third would let a truncated archive load as a
/// deliberate omission.
fn evidence<T: serde::de::DeserializeOwned>(
    key: &'static str,
    front: &FrontMatter,
    bytes: &Option<Vec<u8>>,
    kind: &str,
) -> Result<Option<Vec<T>>, LoadError> {
    let declared = match key {
        "logdata" => front.logdata.as_ref(),
        _ => front.spandata.as_ref(),
    };
    match (declared, bytes) {
        (None, _) => Ok(None),
        (Some(d), None) => Err(LoadError::DeclaredButMissing(d.file.clone())),
        (Some(d), Some(raw)) => {
            // See the document above: a corrupted byte must be an error, not a
            // replacement character inside a record that then parses.
            let text = String::from_utf8(raw.clone()).map_err(|e| {
                LoadError::BadRecords(format!("`{}` is not valid UTF-8: {e}", d.file))
            })?;
            let mut lines = text.lines();
            // The writer emits a header even for zero records, so an absent one
            // is positive evidence of truncation rather than an empty file.
            let header = lines
                .next()
                .ok_or_else(|| LoadError::BadHeader(format!("`{}` has no header line", d.file)))?;
            check_header(&d.file, header, kind)?;

            let mut out = Vec::new();
            for (n, line) in lines.enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                // Checked INSIDE the loop, not after it. The ceiling protects
                // the ring, but a 40M-record file was fully parsed into a `Vec`
                // before the check downstream rejected it — so the allocation
                // the ceiling exists to prevent had already happened.
                if out.len() >= MAX_CASE_RECORDS {
                    return Err(LoadError::TooManyRecords(out.len() + 1));
                }
                let rec: T = serde_json::from_str(line).map_err(|e| {
                    LoadError::BadRecords(format!("`{}` record {n}: {e}", d.file))
                })?;
                out.push(rec);
            }
            if out.len() as u64 != d.records {
                return Err(LoadError::CountMismatch {
                    key,
                    declared: d.records,
                    parsed: out.len(),
                });
            }
            Ok(Some(out))
        }
    }
}

fn check_header(file: &str, header: &str, want_kind: &str) -> Result<(), LoadError> {
    #[derive(serde::Deserialize)]
    struct H {
        logmon_format: u32,
        kind: String,
    }
    let h: H = serde_json::from_str(header)
        .map_err(|e| LoadError::BadHeader(format!("`{file}` header is not readable: {e}")))?;
    if h.logmon_format > super::FORMAT_VERSION {
        return Err(LoadError::BadHeader(format!(
            "`{file}` was written by a newer logmon (format {} > {})",
            h.logmon_format,
            super::FORMAT_VERSION
        )));
    }
    // **The kind is checked, and revision 1 forgot it.** Without this a case
    // whose two data files were swapped loads cleanly with logs in the span
    // store, and the version rule cannot catch it — both are version 1.
    if h.kind != want_kind {
        return Err(LoadError::BadHeader(format!(
            "`{file}` holds `{}` records but is named as `{want_kind}`; this \
             case's data files have been swapped or renamed",
            h.kind
        )));
    }
    Ok(())
}

/// Records must be ascending, distinct and inside the declared window.
///
/// The stores assume exactly this: `context_by_seq` locates by `position()` and
/// slices, and `seq_set` is a `HashSet`, so a duplicate is kept by the deque and
/// collapsed by the set — after which `len()` and `contains_seq` disagree and
/// every windowed read is subtly wrong.
fn validate_seqs<T: HasSeq>(
    key: &'static str,
    records: Option<&[T]>,
    front: &FrontMatter,
) -> Result<(), LoadError> {
    let Some(records) = records else {
        return Ok(());
    };
    let (from, to) = (front.seq_range.from, front.seq_range.to);
    if from > to {
        return Err(LoadError::BadRecords(format!(
            "the declared window is inverted: from {from} to {to}"
        )));
    }
    let mut prev: Option<u64> = None;
    for r in records {
        let seq = r.seq();
        if seq < from || seq > to {
            return Err(LoadError::BadRecords(format!(
                "`{key}` holds seq {seq}, outside the declared window {from}–{to}"
            )));
        }
        if let Some(p) = prev {
            if seq <= p {
                return Err(LoadError::BadRecords(format!(
                    "`{key}` seqs are not ascending and distinct: {seq} follows {p}"
                )));
            }
        }
        prev = Some(seq);
    }
    Ok(())
}

pub trait HasSeq {
    fn seq(&self) -> u64;
}
impl HasSeq for LogEntry {
    fn seq(&self) -> u64 {
        self.seq
    }
}
impl HasSeq for SpanEntry {
    fn seq(&self) -> u64 {
        self.seq
    }
}

#[cfg(test)]
mod tests;
