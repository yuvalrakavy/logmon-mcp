//! Reading a case document's front matter — the inverse of the emitter in
//! [`super::document::render`], and the loader's entire entry point.
//!
//! **Why a hand-rolled parser rather than a YAML crate.** The front matter is a
//! FIXED 14-key schema, not arbitrary YAML, and a general parser's generality is
//! a liability here: it would accept documents this emitter can never produce,
//! so a corrupted case would parse into something plausible instead of failing
//! loudly. It would also be the workspace's first non-JSON serialization
//! dependency — everything the broker persists today is `serde_json`.
//!
//! **What pins this to the emitter.** Not the parser's care: the round-trip
//! tests below, which run a real `CaseInput` through the real renderer and
//! parse what comes out. Emitter and parser are two halves of one contract and
//! the only thing that can keep them together is a test that exercises both.
//! The escaping half — the genuinely risky part — is pinned by a property test
//! over generated strings rather than by examples.
//!
//! The grammar this accepts, which is exactly what `front_matter` emits:
//!
//! ```text
//! ---
//! key: <value>            one key per line, no value spans lines
//! ---
//!
//! <value> ::= "quoted"    with the escape set `yaml_str` produces
//!           | {k: v, ...} flow mapping
//!           | [v, ...]    flow sequence
//!           | null | true | false | 123 | bare-word
//! ```

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use logmon_broker_protocol::EvidenceVerdict;

use super::FORMAT_VERSION;

/// Why a case document could not be read.
///
/// Every variant names the key or position at fault: a case that fails to load
/// is usually a case someone needs to salvage, and "malformed front matter" is
/// not a starting point for that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// No `---` fence, or no closing one.
    NoFrontMatter,
    /// A line inside the fences that is not `key: value`.
    MalformedLine { line: usize, text: String },
    /// A required key never appeared.
    MissingKey(&'static str),
    /// A key appeared with a shape this schema does not allow.
    WrongType {
        key: &'static str,
        wanted: &'static str,
        got: String,
    },
    /// A quoted scalar carried an escape `yaml_str` never emits.
    BadEscape { at: usize, seq: String },
    /// A quoted scalar never closed.
    UnterminatedString { at: usize },
    /// Structural junk inside a flow collection.
    BadFlow { at: usize, what: String },
    /// A timestamp that is not RFC 3339.
    BadTimestamp { key: &'static str, value: String },
    /// `verdict:` carried a word that is not one of the four.
    BadVerdict(String),
    /// Written by a newer logmon than this one can read.
    FormatTooNew { found: u32, known: u32 },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFrontMatter => write!(
                f,
                "no front matter: a case document opens with `---`, a block of \
                 keys, and a closing `---`"
            ),
            Self::MalformedLine { line, text } => write!(
                f,
                "front matter line {line} is not `key: value`: {text:?}"
            ),
            Self::MissingKey(k) => write!(f, "front matter is missing `{k}`"),
            Self::WrongType { key, wanted, got } => {
                write!(f, "`{key}` should be {wanted}, found {got}")
            }
            Self::BadEscape { at, seq } => write!(
                f,
                "unknown escape {seq:?} at offset {at} — this document was not \
                 written by logmon's own escaper, or it has been edited"
            ),
            Self::UnterminatedString { at } => {
                write!(f, "unterminated quoted string starting at offset {at}")
            }
            Self::BadFlow { at, what } => write!(f, "malformed {what} at offset {at}"),
            Self::BadTimestamp { key, value } => {
                write!(f, "`{key}` is not an RFC 3339 timestamp: {value:?}")
            }
            Self::BadVerdict(v) => write!(
                f,
                "`verdict` is {v:?}, not one of complete/evicted/filtered/cannot_verify"
            ),
            Self::FormatTooNew { found, known } => write!(
                f,
                "case was written by a newer logmon (format {found} > {known}); \
                 this broker cannot read it without losing meaning"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

// ---------------------------------------------------------------------------
// The value model — deliberately just what the grammar above can express
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    /// A `"quoted"` scalar, already un-escaped.
    Str(String),
    /// An unquoted word: `complete`, `seq`, a stem, `null`, `true`, a number.
    /// Kept as text because only the field's own accessor knows what it wants.
    Bare(String),
    Map(BTreeMap<String, Value>),
    Seq(Vec<Value>),
}

impl Value {
    fn type_name(&self) -> String {
        match self {
            Self::Str(s) => format!("the string {s:?}"),
            Self::Bare(s) => format!("the bare word {s:?}"),
            Self::Map(_) => "a mapping".into(),
            Self::Seq(_) => "a sequence".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Un-escaping — the mirror of `yaml_str`, and the risky half of this module
// ---------------------------------------------------------------------------

/// Reverse [`crate::collector::document::yaml_str`].
///
/// **Strict by construction: an escape that `yaml_str` never emits is an
/// error.** Being permissive here is how a corrupted document turns into a
/// plausible-looking record — the failure this whole module is shaped to avoid.
fn unescape(raw: &str, base: usize) -> Result<String, ParseError> {
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.char_indices();
    while let Some((i, c)) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some((_, esc)) = it.next() else {
            return Err(ParseError::BadEscape {
                at: base + i,
                seq: "\\".into(),
            });
        };
        match esc {
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            // `\N`, `\L`, `\P` — the three line breaks YAML recognises beyond
            // \n and \r. They arrive as ordinary characters from any app that
            // logs Unicode, so they are not exotic.
            'N' => out.push('\u{85}'),
            'L' => out.push('\u{2028}'),
            'P' => out.push('\u{2029}'),
            'x' => {
                let hi = it.next().map(|(_, c)| c);
                let lo = it.next().map(|(_, c)| c);
                let (Some(hi), Some(lo)) = (hi, lo) else {
                    return Err(ParseError::BadEscape {
                        at: base + i,
                        seq: "\\x".into(),
                    });
                };
                let n = u32::from_str_radix(&format!("{hi}{lo}"), 16).map_err(|_| {
                    ParseError::BadEscape {
                        at: base + i,
                        seq: format!("\\x{hi}{lo}"),
                    }
                })?;
                match char::from_u32(n) {
                    Some(c) => out.push(c),
                    None => {
                        return Err(ParseError::BadEscape {
                            at: base + i,
                            seq: format!("\\x{hi}{lo}"),
                        })
                    }
                }
            }
            other => {
                return Err(ParseError::BadEscape {
                    at: base + i,
                    seq: format!("\\{other}"),
                })
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The scanner
// ---------------------------------------------------------------------------

struct Cur {
    chars: Vec<char>,
    i: usize,
    /// Byte offset of `chars[0]` in the whole document, for error messages that
    /// a human can locate.
    base: usize,
}

impl Cur {
    fn new(s: &str, base: usize) -> Self {
        Self {
            chars: s.chars().collect(),
            i: 0,
            base,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c == ' ' || c == '\t') {
            self.i += 1;
        }
    }

    fn at(&self) -> usize {
        self.base + self.i
    }

    /// A `"..."` scalar. Quotes and escapes only — `}` and `,` inside a string
    /// must not end the enclosing collection, which is the whole reason this is
    /// a scanner and not a `split(',')`.
    fn quoted(&mut self) -> Result<String, ParseError> {
        let start = self.at();
        debug_assert_eq!(self.peek(), Some('"'));
        self.i += 1;
        let mut raw = String::new();
        loop {
            match self.peek() {
                None => return Err(ParseError::UnterminatedString { at: start }),
                Some('"') => {
                    self.i += 1;
                    return unescape(&raw, start + 1);
                }
                Some('\\') => {
                    raw.push('\\');
                    self.i += 1;
                    match self.peek() {
                        None => return Err(ParseError::UnterminatedString { at: start }),
                        Some(c) => {
                            raw.push(c);
                            self.i += 1;
                        }
                    }
                }
                Some(c) => {
                    raw.push(c);
                    self.i += 1;
                }
            }
        }
    }

    /// An unquoted token: everything up to a `,`, `}`, `]` or end of input.
    fn bare(&mut self) -> String {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c == ',' || c == '}' || c == ']' {
                break;
            }
            self.i += 1;
        }
        self.chars[start..self.i].iter().collect::<String>()
            .trim()
            .to_string()
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        self.skip_ws();
        match self.peek() {
            Some('"') => Ok(Value::Str(self.quoted()?)),
            Some('{') => self.flow_map(),
            Some('[') => self.flow_seq(),
            _ => Ok(Value::Bare(self.bare())),
        }
    }

    fn flow_map(&mut self) -> Result<Value, ParseError> {
        let start = self.at();
        self.i += 1; // '{'
        let mut map = BTreeMap::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => {
                    return Err(ParseError::BadFlow {
                        at: start,
                        what: "mapping (never closed)".into(),
                    })
                }
                Some('}') => {
                    self.i += 1;
                    return Ok(Value::Map(map));
                }
                Some(',') => {
                    self.i += 1;
                }
                _ => {
                    // `asserted` quotes its keys; every other mapping writes
                    // them bare. Both are ordinary here.
                    let key = if self.peek() == Some('"') {
                        self.quoted()?
                    } else {
                        let start_k = self.i;
                        while let Some(c) = self.peek() {
                            if c == ':' || c == ',' || c == '}' {
                                break;
                            }
                            self.i += 1;
                        }
                        self.chars[start_k..self.i]
                            .iter()
                            .collect::<String>()
                            .trim()
                            .to_string()
                    };
                    self.skip_ws();
                    if self.peek() != Some(':') {
                        return Err(ParseError::BadFlow {
                            at: self.at(),
                            what: format!("mapping (no `:` after key {key:?})"),
                        });
                    }
                    self.i += 1;
                    let v = self.value()?;
                    map.insert(key, v);
                }
            }
        }
    }

    fn flow_seq(&mut self) -> Result<Value, ParseError> {
        let start = self.at();
        self.i += 1; // '['
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => {
                    return Err(ParseError::BadFlow {
                        at: start,
                        what: "sequence (never closed)".into(),
                    })
                }
                Some(']') => {
                    self.i += 1;
                    return Ok(Value::Seq(items));
                }
                Some(',') => {
                    self.i += 1;
                }
                _ => items.push(self.value()?),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The typed front matter
// ---------------------------------------------------------------------------

/// A case document's front matter, which the emitter calls "the index surface:
/// fixed-schema, small, grep-able" — and which is therefore also the machine
/// contract the loader reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontMatter {
    pub case: String,
    pub logmon_format: u32,
    pub captured_at: DateTime<Utc>,
    pub domain: String,
    /// `null` in the document when the capturing broker had no config dir.
    pub incarnation: Option<String>,
    pub reason: String,
    pub anchor: AnchorRef,
    pub headline: String,
    pub verdict: EvidenceVerdict,
    pub seq_range: SeqRange,
    pub logdata: FileRef,
    pub spandata: FileRef,
    pub provenance: Provenance,
    /// Document-scoped `@` facts, asserted about this capture alone.
    pub asserted: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorRef {
    pub kind: String,
    pub value: String,
    pub seq: u64,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqRange {
    pub from: u64,
    pub to: u64,
    pub requested_before_missing: usize,
    pub requested_after_missing: usize,
    pub clamped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    pub file: String,
    pub records: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// Verbatim `"N of M"`. Kept as text because it is a human-facing summary
    /// and re-deriving it here would be a second opinion nobody asked for.
    pub core: String,
    pub missing: Vec<String>,
}

// ---------------------------------------------------------------------------
// Field extraction
// ---------------------------------------------------------------------------

fn take<'a>(
    map: &'a BTreeMap<String, Value>,
    key: &'static str,
) -> Result<&'a Value, ParseError> {
    map.get(key).ok_or(ParseError::MissingKey(key))
}

fn as_text(v: &Value, key: &'static str) -> Result<String, ParseError> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        Value::Bare(s) => Ok(s.clone()),
        other => Err(ParseError::WrongType {
            key,
            wanted: "a scalar",
            got: other.type_name(),
        }),
    }
}

fn as_u64(v: &Value, key: &'static str) -> Result<u64, ParseError> {
    let t = as_text(v, key)?;
    t.parse::<u64>().map_err(|_| ParseError::WrongType {
        key,
        wanted: "a non-negative integer",
        got: format!("{t:?}"),
    })
}

fn as_bool(v: &Value, key: &'static str) -> Result<bool, ParseError> {
    match as_text(v, key)?.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(ParseError::WrongType {
            key,
            wanted: "true or false",
            got: format!("{other:?}"),
        }),
    }
}

fn as_map<'a>(
    v: &'a Value,
    key: &'static str,
) -> Result<&'a BTreeMap<String, Value>, ParseError> {
    match v {
        Value::Map(m) => Ok(m),
        other => Err(ParseError::WrongType {
            key,
            wanted: "a mapping",
            got: other.type_name(),
        }),
    }
}

fn as_time(v: &Value, key: &'static str) -> Result<DateTime<Utc>, ParseError> {
    let t = as_text(v, key)?;
    DateTime::parse_from_rfc3339(&t)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| ParseError::BadTimestamp { key, value: t })
}

/// Pull a key out of a nested flow mapping, reporting the OUTER key so the
/// message locates the fault in the document rather than in this module.
fn sub<'a>(
    m: &'a BTreeMap<String, Value>,
    outer: &'static str,
    inner: &str,
) -> Result<&'a Value, ParseError> {
    m.get(inner).ok_or(ParseError::WrongType {
        key: outer,
        wanted: "a mapping containing all its fields",
        got: format!("a mapping with no {inner:?}"),
    })
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

/// Split the `---` fenced block off the front of a case document.
fn fenced(doc: &str) -> Result<(&str, usize), ParseError> {
    let body = doc.strip_prefix("---\n").ok_or(ParseError::NoFrontMatter)?;
    let end = body.find("\n---").ok_or(ParseError::NoFrontMatter)?;
    Ok((&body[..end], 4))
}

/// Parse a case document's front matter.
///
/// **Unknown keys are ignored; missing required keys are an error.** That is
/// forward compatibility in the additive direction only, matching the
/// discipline the collector path already documents: a field added later must
/// not stop an older broker reading the file, but a field this one needs and
/// cannot find is not something to guess at.
pub fn parse_front_matter(doc: &str) -> Result<FrontMatter, ParseError> {
    let (block, base) = fenced(doc)?;

    let mut top: BTreeMap<String, Value> = BTreeMap::new();
    let mut offset = base;
    for (n, line) in block.lines().enumerate() {
        let len = line.len() + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            offset += len;
            continue;
        }
        let Some((key, rest)) = trimmed.split_once(':') else {
            return Err(ParseError::MalformedLine {
                line: n + 1,
                text: trimmed.to_string(),
            });
        };
        let mut cur = Cur::new(rest, offset + key.len() + 1);
        let value = cur.value()?;
        top.insert(key.trim().to_string(), value);
        offset += len;
    }

    // Version first: everything below is an interpretation of bytes whose
    // meaning this check is what establishes.
    let logmon_format = as_u64(take(&top, "logmon_format")?, "logmon_format")? as u32;
    if logmon_format > FORMAT_VERSION {
        return Err(ParseError::FormatTooNew {
            found: logmon_format,
            known: FORMAT_VERSION,
        });
    }

    let anchor_m = as_map(take(&top, "anchor")?, "anchor")?;
    let anchor = AnchorRef {
        kind: as_text(sub(anchor_m, "anchor", "kind")?, "anchor")?,
        value: as_text(sub(anchor_m, "anchor", "value")?, "anchor")?,
        seq: as_u64(sub(anchor_m, "anchor", "seq")?, "anchor")?,
        at: as_time(sub(anchor_m, "anchor", "at")?, "anchor")?,
    };

    let sr = as_map(take(&top, "seq_range")?, "seq_range")?;
    let seq_range = SeqRange {
        from: as_u64(sub(sr, "seq_range", "from")?, "seq_range")?,
        to: as_u64(sub(sr, "seq_range", "to")?, "seq_range")?,
        requested_before_missing: as_u64(
            sub(sr, "seq_range", "requested_before_missing")?,
            "seq_range",
        )? as usize,
        requested_after_missing: as_u64(
            sub(sr, "seq_range", "requested_after_missing")?,
            "seq_range",
        )? as usize,
        clamped: as_bool(sub(sr, "seq_range", "clamped")?, "seq_range")?,
    };

    let file_ref = |key: &'static str| -> Result<FileRef, ParseError> {
        let m = as_map(take(&top, key)?, key)?;
        Ok(FileRef {
            file: as_text(sub(m, key, "file")?, key)?,
            records: as_u64(sub(m, key, "records")?, key)?,
        })
    };

    let prov_m = as_map(take(&top, "provenance")?, "provenance")?;
    let provenance = Provenance {
        core: as_text(sub(prov_m, "provenance", "core")?, "provenance")?,
        missing: match sub(prov_m, "provenance", "missing")? {
            Value::Seq(items) => items
                .iter()
                .map(|v| as_text(v, "provenance"))
                .collect::<Result<Vec<_>, _>>()?,
            other => {
                return Err(ParseError::WrongType {
                    key: "provenance",
                    wanted: "a mapping whose `missing` is a sequence",
                    got: other.type_name(),
                })
            }
        },
    };

    let asserted = match top.get("asserted") {
        // Absent is not the same as empty, but both mean "nothing asserted",
        // and a document written before this key existed is still readable.
        None => BTreeMap::new(),
        Some(v) => as_map(v, "asserted")?
            .iter()
            .map(|(k, v)| as_text(v, "asserted").map(|s| (k.clone(), s)))
            .collect::<Result<BTreeMap<_, _>, _>>()?,
    };

    let incarnation = match as_text(take(&top, "incarnation")?, "incarnation")? {
        s if s == "null" => None,
        s => Some(s),
    };

    let verdict = match as_text(take(&top, "verdict")?, "verdict")?.as_str() {
        "complete" => EvidenceVerdict::Complete,
        "evicted" => EvidenceVerdict::Evicted,
        "filtered" => EvidenceVerdict::Filtered,
        "cannot_verify" => EvidenceVerdict::CannotVerify,
        other => return Err(ParseError::BadVerdict(other.to_string())),
    };

    Ok(FrontMatter {
        case: as_text(take(&top, "case")?, "case")?,
        logmon_format,
        captured_at: as_time(take(&top, "captured_at")?, "captured_at")?,
        domain: as_text(take(&top, "domain")?, "domain")?,
        incarnation,
        reason: as_text(take(&top, "reason")?, "reason")?,
        anchor,
        headline: as_text(take(&top, "headline")?, "headline")?,
        verdict,
        seq_range,
        logdata: file_ref("logdata")?,
        spandata: file_ref("spandata")?,
        provenance,
        asserted,
    })
}

// ---------------------------------------------------------------------------
// The machine-readable registry block
// ---------------------------------------------------------------------------

/// §5's machine half — the facts `load_case` restores, unrounded.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RegistryBlock {
    pub logmon_format: u32,
    pub facts: Vec<super::document::RegistryFact>,
    /// Facts the capture held but the document's budget could not carry.
    /// **Non-zero means the restored provenance is a subset**, and a loader must
    /// say so rather than presenting it as the registry.
    pub dropped: usize,
}

/// Parse the registry block, if the document has one.
///
/// `Ok(None)` is a real answer, not a failure: a case written before this block
/// existed is still a valid case, and the loader's job is to report that its
/// provenance could not be restored — not to refuse the case or, worse, to
/// reconstruct facts from the rendered table and present the rounding as exact.
pub fn parse_registry(doc: &str) -> Result<Option<RegistryBlock>, ParseError> {
    let fence = format!("```json {}", super::document::REGISTRY_BLOCK_TAG);
    let mut lines = doc.lines();
    let Some(_) = lines.find(|l| l.trim_end() == fence) else {
        return Ok(None);
    };
    let Some(payload) = lines.next() else {
        return Err(ParseError::BadFlow {
            at: 0,
            what: "registry block (fence opened at end of document)".into(),
        });
    };
    let block: RegistryBlock =
        serde_json::from_str(payload).map_err(|e| ParseError::BadFlow {
            at: 0,
            what: format!("registry block ({e})"),
        })?;
    // Same policy as the front matter, and checked here too: the block is its
    // own carrier and a document could in principle be assembled from halves.
    if block.logmon_format > FORMAT_VERSION {
        return Err(ParseError::FormatTooNew {
            found: block.logmon_format,
            known: FORMAT_VERSION,
        });
    }
    Ok(Some(block))
}

#[cfg(test)]
mod tests;
