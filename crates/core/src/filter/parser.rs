use crate::gelf::message::Level;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ParsedFilter {
    All,
    None,
    Qualifiers(Vec<Qualifier>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Qualifier {
    BarePattern(Pattern),
    SelectorPattern(Selector, Pattern),
    LevelFilter { op: LevelOp, level: Level },
    DurationFilter(DurationOp, f64),
    BookmarkFilter { op: BookmarkOp, name: String },
    /// Read-and-advance bookmark reference. Resolved to `SeqFilter` by
    /// `bookmark_resolver::resolve_bookmarks` (which also acquires the
    /// CursorCommit handle from the BookmarkStore).
    CursorFilter { name: String },
    /// Internal-only: produced by `resolve_bookmarks` from `BookmarkFilter`
    /// (and from `CursorFilter` once Task 6/7 lands). Never emitted by the
    /// parser, never serialized in user-facing wire shapes.
    /// Uses `SeqOp` (strict `Gt`/`Lt`) — distinct from `BookmarkOp` because the
    /// cursor design resolves `b>=name` / `b<=name` to STRICT `>` / `<` against
    /// the bookmark's seq (not ≥/≤).
    SeqFilter { op: SeqOp, value: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DurationOp {
    Gte,
    Lte,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum BookmarkOp {
    Gte,
    Lte,
}

/// Strict comparison op for the internal-only `SeqFilter` qualifier.
/// Distinct from `BookmarkOp` because the spec semantics for `b>=name` and
/// `b<=name` resolve to STRICT `>` / `<` against the bookmark's seq (not ≥/≤).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SeqOp {
    Gt,
    Lt,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Substring(String), // stored lowercase for case-insensitive matching
    Regex {
        source: String,
        compiled: regex::Regex,
        case_insensitive: bool,
    },
}

// Custom Serialize/Deserialize for Pattern since regex::Regex doesn't implement them
impl Serialize for Pattern {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Pattern::Substring(s) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "substring")?;
                map.serialize_entry("value", s)?;
                map.end()
            }
            Pattern::Regex {
                source,
                case_insensitive,
                ..
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "regex")?;
                map.serialize_entry("source", source)?;
                map.serialize_entry("case_insensitive", case_insensitive)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Pattern {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{self, MapAccess, Visitor};
        use std::fmt;

        struct PatternVisitor;

        impl<'de> Visitor<'de> for PatternVisitor {
            type Value = Pattern;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a Pattern map")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut r#type: Option<String> = None;
                let mut value: Option<String> = None;
                let mut source: Option<String> = None;
                let mut case_insensitive: Option<bool> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => r#type = Some(map.next_value()?),
                        "value" => value = Some(map.next_value()?),
                        "source" => source = Some(map.next_value()?),
                        "case_insensitive" => case_insensitive = Some(map.next_value()?),
                        _ => {
                            let _ = map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }

                match r#type.as_deref() {
                    Some("substring") => {
                        let v = value.ok_or_else(|| de::Error::missing_field("value"))?;
                        Ok(Pattern::Substring(v))
                    }
                    Some("regex") => {
                        let src = source.ok_or_else(|| de::Error::missing_field("source"))?;
                        let ci = case_insensitive.unwrap_or(false);
                        let pattern = if ci {
                            format!("(?i){}", src)
                        } else {
                            src.clone()
                        };
                        let compiled = regex::Regex::new(&pattern)
                            .map_err(|e| de::Error::custom(e.to_string()))?;
                        Ok(Pattern::Regex {
                            source: src,
                            compiled,
                            case_insensitive: ci,
                        })
                    }
                    other => Err(de::Error::custom(format!(
                        "unknown pattern type: {:?}",
                        other
                    ))),
                }
            }
        }

        deserializer.deserialize_map(PatternVisitor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Selector {
    Message,                   // m
    FullMessage,               // fm
    MessageOrFull,             // mfm
    Host,                      // h
    Facility,                  // fa
    File,                      // fi
    Line,                      // ln
    SpanName,                  // sn
    ServiceName,               // sv
    SpanStatus,                // st
    SpanKind,                  // sk
    AdditionalField(String),   // anything else
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LevelOp {
    Eq,
    Gte,
    Lte,
}

#[derive(Debug, Error)]
pub enum FilterParseError {
    #[error("empty filter string")]
    Empty,
    #[error("invalid regex: {0}")]
    InvalidRegex(#[from] regex::Error),
    #[error("unknown level: {0}")]
    UnknownLevel(String),
    #[error("invalid level filter syntax: {0}")]
    InvalidLevelSyntax(String),
    #[error("unclosed quote")]
    UnclosedQuote,
    #[error("cannot mix log and span selectors in the same filter")]
    MixedLogSpanSelectors,
    #[error("empty bookmark name")]
    EmptyBookmarkName,
    #[error("invalid bookmark name: {0}")]
    InvalidBookmarkName(String),
    #[error("only one cursor qualifier permitted per filter")]
    MultipleCursorQualifiers,
    #[error("c<= is not a valid qualifier; cursors only support read-and-advance via c>=. For a read-only snapshot of past records, use b<=name.")]
    CursorLteNotSupported,
}

/// Validate a bookmark name as it appears inside a DSL filter token.
/// Allowed: ASCII alphanumerics, '-', '_', '/'. The '/' enables qualified
/// names like "session-A/before".
fn is_valid_bookmark_token(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'/')
}

/// Split the input on commas, but respect double-quoted strings (don't split inside quotes).
fn split_on_commas(input: &str) -> Result<Vec<String>, FilterParseError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;

    for ch in input.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                current.push(ch);
            }
            ',' if !in_quote => {
                tokens.push(current.clone());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if in_quote {
        return Err(FilterParseError::UnclosedQuote);
    }

    tokens.push(current);
    Ok(tokens)
}

/// Parse a pattern string (could be a regex `/pattern/[i]` or a plain substring).
fn parse_pattern(s: &str) -> Result<Pattern, FilterParseError> {
    if let Some(inner) = s.strip_prefix('/') {
        // Regex: find the closing /
        let (source, case_insensitive) = if let Some(pos) = inner.rfind('/') {
            let src = &inner[..pos];
            let flags = &inner[pos + 1..];
            let ci = flags.contains('i');
            (src.to_string(), ci)
        } else {
            // No closing slash — treat the whole thing (minus leading /) as pattern without flags
            (inner.to_string(), false)
        };

        let pattern = if case_insensitive {
            format!("(?i){}", source)
        } else {
            source.clone()
        };
        let compiled = regex::Regex::new(&pattern)?;

        Ok(Pattern::Regex {
            source,
            compiled,
            case_insensitive,
        })
    } else {
        // Substring: store lowercased
        Ok(Pattern::Substring(s.to_lowercase()))
    }
}

/// Map a selector string to a Selector enum variant.
fn parse_selector(s: &str) -> Selector {
    match s {
        "m" => Selector::Message,
        "fm" => Selector::FullMessage,
        "mfm" => Selector::MessageOrFull,
        "h" => Selector::Host,
        "fa" => Selector::Facility,
        "fi" => Selector::File,
        "ln" => Selector::Line,
        "sn" => Selector::SpanName,
        "sv" => Selector::ServiceName,
        "st" => Selector::SpanStatus,
        "sk" => Selector::SpanKind,
        other => Selector::AdditionalField(other.to_string()),
    }
}

/// Parse a single token into a Qualifier.
fn parse_token(token: &str) -> Result<Qualifier, FilterParseError> {
    let token = token.trim();

    // Level filter: l>=X, l<=X, l=X  (must check before general = handling)
    if let Some(rest) = token.strip_prefix("l>=") {
        let level = Level::from_name(rest)
            .ok_or_else(|| FilterParseError::UnknownLevel(rest.to_string()))?;
        return Ok(Qualifier::LevelFilter {
            op: LevelOp::Gte,
            level,
        });
    }
    if let Some(rest) = token.strip_prefix("l<=") {
        let level = Level::from_name(rest)
            .ok_or_else(|| FilterParseError::UnknownLevel(rest.to_string()))?;
        return Ok(Qualifier::LevelFilter {
            op: LevelOp::Lte,
            level,
        });
    }
    if let Some(rest) = token.strip_prefix("l=") {
        let level = Level::from_name(rest)
            .ok_or_else(|| FilterParseError::UnknownLevel(rest.to_string()))?;
        return Ok(Qualifier::LevelFilter {
            op: LevelOp::Eq,
            level,
        });
    }

    // Duration filter: d>=N, d<=N
    if let Some(rest) = token.strip_prefix("d>=") {
        let value: f64 = rest.parse()
            .map_err(|_| FilterParseError::InvalidLevelSyntax(format!("invalid duration value: {}", rest)))?;
        return Ok(Qualifier::DurationFilter(DurationOp::Gte, value));
    }
    if let Some(rest) = token.strip_prefix("d<=") {
        let value: f64 = rest.parse()
            .map_err(|_| FilterParseError::InvalidLevelSyntax(format!("invalid duration value: {}", rest)))?;
        return Ok(Qualifier::DurationFilter(DurationOp::Lte, value));
    }

    // Bookmark filter: b>=NAME, b<=NAME
    if let Some(rest) = token.strip_prefix("b>=") {
        let name = rest.trim();
        if name.is_empty() {
            return Err(FilterParseError::EmptyBookmarkName);
        }
        if !is_valid_bookmark_token(name) {
            return Err(FilterParseError::InvalidBookmarkName(name.to_string()));
        }
        return Ok(Qualifier::BookmarkFilter {
            op: BookmarkOp::Gte,
            name: name.to_string(),
        });
    }
    if let Some(rest) = token.strip_prefix("b<=") {
        let name = rest.trim();
        if name.is_empty() {
            return Err(FilterParseError::EmptyBookmarkName);
        }
        if !is_valid_bookmark_token(name) {
            return Err(FilterParseError::InvalidBookmarkName(name.to_string()));
        }
        return Ok(Qualifier::BookmarkFilter {
            op: BookmarkOp::Lte,
            name: name.to_string(),
        });
    }

    // Cursor filter: c>=NAME (read-and-advance variant of b>=)
    if let Some(rest) = token.strip_prefix("c>=") {
        let name = rest.trim();
        if name.is_empty() {
            return Err(FilterParseError::EmptyBookmarkName);
        }
        if !is_valid_bookmark_token(name) {
            return Err(FilterParseError::InvalidBookmarkName(name.to_string()));
        }
        return Ok(Qualifier::CursorFilter {
            name: name.to_string(),
        });
    }

    // c<= is intentionally not defined per the cursor design spec.
    // Reject explicitly so users don't silently get a custom-field filter
    // (which is what would happen if c<=foo fell through to selector parsing).
    if token.starts_with("c<=") {
        return Err(FilterParseError::CursorLteNotSupported);
    }

    // Quoted bare pattern: "..."
    if token.starts_with('"') && token.ends_with('"') && token.len() >= 2 {
        let inner = &token[1..token.len() - 1];
        // Quoted patterns are stored as-is (lowercased for case-insensitive matching)
        return Ok(Qualifier::BarePattern(Pattern::Substring(
            inner.to_lowercase(),
        )));
    }

    // Bare regex: /pattern/[i]
    if token.starts_with('/') {
        return Ok(Qualifier::BarePattern(parse_pattern(token)?));
    }

    // Selector=pattern: look for the first `=` sign
    if let Some(eq_pos) = token.find('=') {
        let lhs = token[..eq_pos].trim();
        let rhs = token[eq_pos + 1..].trim();

        // lhs must not be empty; if it's empty treat as bare substring
        if !lhs.is_empty() {
            let selector = parse_selector(lhs);
            // Strip surrounding quotes from pattern value
            let rhs = if rhs.starts_with('"') && rhs.ends_with('"') && rhs.len() >= 2 {
                &rhs[1..rhs.len() - 1]
            } else {
                rhs
            };
            let pattern = parse_pattern(rhs)?;
            return Ok(Qualifier::SelectorPattern(selector, pattern));
        }
    }

    // Bare substring (lowercased)
    Ok(Qualifier::BarePattern(Pattern::Substring(
        token.to_lowercase(),
    )))
}

/// Returns true if the filter uses span-specific selectors or duration filters.
pub fn is_span_filter(filter: &ParsedFilter) -> bool {
    match filter {
        ParsedFilter::All | ParsedFilter::None => false,
        ParsedFilter::Qualifiers(qs) => qs.iter().any(is_span_qualifier),
    }
}

fn is_span_qualifier(q: &Qualifier) -> bool {
    match q {
        Qualifier::SelectorPattern(sel, _) => matches!(
            sel,
            Selector::SpanName | Selector::ServiceName | Selector::SpanStatus | Selector::SpanKind
        ),
        Qualifier::DurationFilter(..) => true,
        _ => false,
    }
}

fn is_log_qualifier(q: &Qualifier) -> bool {
    match q {
        Qualifier::SelectorPattern(sel, _) => matches!(
            sel,
            Selector::Message
                | Selector::FullMessage
                | Selector::MessageOrFull
                | Selector::Host
                | Selector::Facility
                | Selector::File
                | Selector::Line
        ),
        Qualifier::LevelFilter { .. } => true,
        _ => false,
    }
}

pub fn parse_filter(input: &str) -> Result<ParsedFilter, FilterParseError> {
    let input = input.trim();

    if input.is_empty() {
        return Err(FilterParseError::Empty);
    }

    if input == "ALL" {
        return Ok(ParsedFilter::All);
    }

    if input == "NONE" {
        return Ok(ParsedFilter::None);
    }

    let tokens = split_on_commas(input)?;
    let mut qualifiers = Vec::with_capacity(tokens.len());

    for token in tokens {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        qualifiers.push(parse_token(token)?);
    }

    if qualifiers.is_empty() {
        return Err(FilterParseError::Empty);
    }

    // Validate: cannot mix log-specific and span-specific selectors
    let has_span = qualifiers.iter().any(is_span_qualifier);
    let has_log = qualifiers.iter().any(is_log_qualifier);
    if has_span && has_log {
        return Err(FilterParseError::MixedLogSpanSelectors);
    }

    // Validate: only one cursor qualifier permitted
    let cursor_count = qualifiers
        .iter()
        .filter(|q| matches!(q, Qualifier::CursorFilter { .. }))
        .count();
    if cursor_count > 1 {
        return Err(FilterParseError::MultipleCursorQualifiers);
    }

    Ok(ParsedFilter::Qualifiers(qualifiers))
}

#[cfg(test)]
mod bookmark_tests {
    use super::*;

    fn parse_one(s: &str) -> Qualifier {
        match parse_filter(s).unwrap() {
            ParsedFilter::Qualifiers(mut qs) => qs.remove(0),
            _ => panic!("expected qualifiers"),
        }
    }

    #[test]
    fn parses_bookmark_gte_bare_name() {
        match parse_one("b>=before") {
            Qualifier::BookmarkFilter { op: BookmarkOp::Gte, name } => {
                assert_eq!(name, "before");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_bookmark_lte_bare_name() {
        match parse_one("b<=after") {
            Qualifier::BookmarkFilter { op: BookmarkOp::Lte, name } => {
                assert_eq!(name, "after");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_bookmark_qualified_name() {
        match parse_one("b>=session-A/before") {
            Qualifier::BookmarkFilter { op: BookmarkOp::Gte, name } => {
                assert_eq!(name, "session-A/before");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn empty_bookmark_name_errors() {
        let err = parse_filter("b>=").unwrap_err();
        assert!(matches!(err, FilterParseError::EmptyBookmarkName));
    }

    #[test]
    fn invalid_bookmark_char_errors() {
        let err = parse_filter("b>=foo bar").unwrap_err();
        assert!(matches!(err, FilterParseError::InvalidBookmarkName(_)));
    }

    #[test]
    fn bookmark_combines_with_log_qualifier_without_mix_error() {
        let parsed = parse_filter("b>=before, l>=warn").unwrap();
        if let ParsedFilter::Qualifiers(qs) = parsed {
            assert_eq!(qs.len(), 2);
        } else {
            panic!("expected qualifiers");
        }
    }

    #[test]
    fn bookmark_combines_with_span_qualifier_without_mix_error() {
        let parsed = parse_filter("b>=before, d>=100").unwrap();
        if let ParsedFilter::Qualifiers(qs) = parsed {
            assert_eq!(qs.len(), 2);
        } else {
            panic!("expected qualifiers");
        }
    }

    #[test]
    fn bookmark_whitespace_around_name_trimmed() {
        match parse_one("b>=  before  ") {
            Qualifier::BookmarkFilter { name, .. } => assert_eq!(name, "before"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn contains_bookmark_qualifier_detects_b_gte() {
        let parsed = parse_filter("b>=foo, l>=warn").unwrap();
        assert!(contains_bookmark_qualifier(&parsed));
    }

    #[test]
    fn contains_bookmark_qualifier_false_for_plain_filter() {
        let parsed = parse_filter("l>=warn").unwrap();
        assert!(!contains_bookmark_qualifier(&parsed));
    }

    #[test]
    fn contains_bookmark_qualifier_handles_all_and_none() {
        assert!(!contains_bookmark_qualifier(&ParsedFilter::All));
        assert!(!contains_bookmark_qualifier(&ParsedFilter::None));
    }

    #[test]
    fn parses_c_ge_qualifier() {
        let f = parse_filter("c>=mycursor").unwrap();
        if let ParsedFilter::Qualifiers(qs) = f {
            assert!(matches!(
                &qs[0],
                Qualifier::CursorFilter { name } if name == "mycursor"
            ));
        } else {
            panic!("expected Qualifiers");
        }
    }

    #[test]
    fn parses_c_ge_with_session_qualifier() {
        let f = parse_filter("c>=other-sess/cursor1").unwrap();
        if let ParsedFilter::Qualifiers(qs) = f {
            assert!(matches!(
                &qs[0],
                Qualifier::CursorFilter { name } if name == "other-sess/cursor1"
            ));
        } else {
            panic!("expected Qualifiers");
        }
    }

    #[test]
    fn rejects_multiple_cursor_qualifiers() {
        let err = parse_filter("c>=a, c>=b").unwrap_err();
        assert!(
            format!("{err}").contains("only one cursor qualifier"),
            "expected multi-cursor error, got: {err}"
        );
    }

    #[test]
    fn c_le_token_rejected_with_guidance() {
        // c<= is intentionally not defined per the cursor design spec.
        // Must reject explicitly (not fall through to selector pattern parsing)
        // to avoid the user getting a silent no-match filter.
        let err = parse_filter("c<=foo").unwrap_err();
        assert!(
            matches!(err, FilterParseError::CursorLteNotSupported),
            "expected CursorLteNotSupported, got: {err:?}"
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("c<="),
            "error message should mention the offending token, got: {msg}"
        );
        assert!(
            msg.contains("c>=") || msg.contains("b<="),
            "error message should suggest the correct alternative, got: {msg}"
        );
    }

    #[test]
    fn rejects_invalid_cursor_name_chars() {
        let err = parse_filter("c>=bad name").unwrap_err();
        assert!(
            matches!(err, FilterParseError::InvalidBookmarkName(_)),
            "expected InvalidBookmarkName, got: {err:?}"
        );
    }

    #[test]
    fn contains_bookmark_qualifier_matches_cursor_too() {
        let f = parse_filter("c>=foo").unwrap();
        assert!(contains_bookmark_qualifier(&f));
        let f = parse_filter("b>=foo").unwrap();
        assert!(contains_bookmark_qualifier(&f));
        let f = parse_filter("l>=ERROR").unwrap();
        assert!(!contains_bookmark_qualifier(&f));
    }
}

/// Returns true if any qualifier in the filter is a `BookmarkFilter` or `CursorFilter`.
/// Used by registration guards to reject bookmark filters and cursor filters in long-lived
/// registered filters/triggers.
pub fn contains_bookmark_qualifier(filter: &ParsedFilter) -> bool {
    match filter {
        ParsedFilter::All | ParsedFilter::None => false,
        ParsedFilter::Qualifiers(qs) => qs.iter().any(|q| {
            matches!(
                q,
                Qualifier::BookmarkFilter { .. } | Qualifier::CursorFilter { .. }
            )
        }),
    }
}
