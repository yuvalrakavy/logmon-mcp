//! Type-safe filter builder.
//!
//! Wraps the broker's string-based filter DSL (`l>=ERROR, m=foo`, etc.) in a
//! fluent builder so callers don't hand-construct filter strings:
//!
//! ```ignore
//! use logmon_broker_sdk::{Filter, Level};
//! let f = Filter::builder()
//!     .level_at_least(Level::Error)
//!     .message("started")
//!     .build();
//! // → "l>=ERROR, m=started"
//! ```
//!
//! The builder produces strings that round-trip cleanly through
//! `core::filter::parser::parse_filter`. Level vocabulary mirrors what the
//! parser accepts (`ERROR`/`WARN`/`INFO`/`DEBUG`/`TRACE`), not the broader
//! syslog set, so unparseable level names are unrepresentable at the type
//! level.

/// Log severity levels accepted by the broker's filter parser. The wire
/// representation is the variant's uppercase name, matching
/// `core::gelf::message::Level::from_name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

/// Span status used by `st=` filter qualifiers. Distinct from
/// [`logmon_broker_protocol::SpanStatus`] (which carries an error message
/// payload on `Error`); filters operate on the *category*, so the SDK uses a
/// payload-free enum prefixed with `Filter` to avoid name shadowing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterSpanStatus {
    Ok,
    Error,
    Unset,
}

impl FilterSpanStatus {
    fn as_str(self) -> &'static str {
        match self {
            FilterSpanStatus::Ok => "ok",
            FilterSpanStatus::Error => "error",
            FilterSpanStatus::Unset => "unset",
        }
    }
}

/// Span kind used by `sk=` filter qualifiers. Same naming rationale as
/// [`FilterSpanStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterSpanKind {
    Server,
    Client,
    Producer,
    Consumer,
    Internal,
}

impl FilterSpanKind {
    fn as_str(self) -> &'static str {
        match self {
            FilterSpanKind::Server => "server",
            FilterSpanKind::Client => "client",
            FilterSpanKind::Producer => "producer",
            FilterSpanKind::Consumer => "consumer",
            FilterSpanKind::Internal => "internal",
        }
    }
}

/// Entry point — call [`Filter::builder`] to start composing a filter.
pub struct Filter;

impl Filter {
    pub fn builder() -> FilterBuilder {
        FilterBuilder {
            qualifiers: Vec::new(),
        }
    }
}

/// Fluent builder for filter strings. Each method appends one qualifier;
/// [`Self::build`] joins them with `", "` (the broker's qualifier separator).
pub struct FilterBuilder {
    qualifiers: Vec<String>,
}

/// Quote a value if it contains a comma or a double-quote — those are the
/// only characters that would break the broker parser's `split_on_commas`
/// tokenizer (see `core::filter::parser::split_on_commas`). Other characters
/// (spaces, colons, `=`, regex metacharacters) survive unquoted.
fn esc(value: &str) -> String {
    if value.contains(',') || value.contains('"') {
        let escaped = value.replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}

/// Render a regex pattern as `/pattern/[i]`. Strips any leading/trailing `/`
/// from the input so callers may pass either raw bodies (`"panic|unwrap"`)
/// or fully-quoted regex literals (`"/panic|unwrap/"`).
fn regex_lit(pattern: &str, case_insensitive: bool) -> String {
    let body = pattern.trim_matches('/');
    if case_insensitive {
        format!("/{body}/i")
    } else {
        format!("/{body}/")
    }
}

impl FilterBuilder {
    /// Render the accumulated qualifiers into a filter string. Empty builder
    /// returns the empty string (caller should typically use
    /// [`Self::match_all`] if they want "match everything").
    pub fn build(self) -> String {
        if self.qualifiers.is_empty() {
            return String::new();
        }
        self.qualifiers.join(", ")
    }

    pub fn match_all(mut self) -> Self {
        self.qualifiers.push("ALL".into());
        self
    }
    pub fn match_none(mut self) -> Self {
        self.qualifiers.push("NONE".into());
        self
    }

    // ---- Level ----
    pub fn level_at_least(mut self, l: Level) -> Self {
        self.qualifiers.push(format!("l>={}", l.as_str()));
        self
    }
    pub fn level_at_most(mut self, l: Level) -> Self {
        self.qualifiers.push(format!("l<={}", l.as_str()));
        self
    }
    pub fn level_eq(mut self, l: Level) -> Self {
        self.qualifiers.push(format!("l={}", l.as_str()));
        self
    }

    // ---- Bare pattern ----
    pub fn pattern(mut self, sub: &str) -> Self {
        self.qualifiers.push(esc(sub));
        self
    }
    pub fn pattern_regex(mut self, regex: &str, case_insensitive: bool) -> Self {
        self.qualifiers.push(regex_lit(regex, case_insensitive));
        self
    }

    // ---- Log selectors (substring + regex variants) ----
    pub fn message(mut self, sub: &str) -> Self {
        self.qualifiers.push(format!("m={}", esc(sub)));
        self
    }
    pub fn message_regex(mut self, r: &str, ci: bool) -> Self {
        self.qualifiers.push(format!("m={}", regex_lit(r, ci)));
        self
    }
    pub fn full_message(mut self, sub: &str) -> Self {
        self.qualifiers.push(format!("fm={}", esc(sub)));
        self
    }
    pub fn full_message_regex(mut self, r: &str, ci: bool) -> Self {
        self.qualifiers.push(format!("fm={}", regex_lit(r, ci)));
        self
    }
    pub fn message_or_full(mut self, sub: &str) -> Self {
        self.qualifiers.push(format!("mfm={}", esc(sub)));
        self
    }
    pub fn message_or_full_regex(mut self, r: &str, ci: bool) -> Self {
        self.qualifiers.push(format!("mfm={}", regex_lit(r, ci)));
        self
    }
    pub fn host(mut self, sub: &str) -> Self {
        self.qualifiers.push(format!("h={}", esc(sub)));
        self
    }
    pub fn host_regex(mut self, r: &str, ci: bool) -> Self {
        self.qualifiers.push(format!("h={}", regex_lit(r, ci)));
        self
    }
    pub fn facility(mut self, sub: &str) -> Self {
        self.qualifiers.push(format!("fa={}", esc(sub)));
        self
    }
    pub fn facility_regex(mut self, r: &str, ci: bool) -> Self {
        self.qualifiers.push(format!("fa={}", regex_lit(r, ci)));
        self
    }
    pub fn file(mut self, sub: &str) -> Self {
        self.qualifiers.push(format!("fi={}", esc(sub)));
        self
    }
    pub fn file_regex(mut self, r: &str, ci: bool) -> Self {
        self.qualifiers.push(format!("fi={}", regex_lit(r, ci)));
        self
    }
    pub fn line(mut self, n: u32) -> Self {
        self.qualifiers.push(format!("ln={n}"));
        self
    }

    // ---- Span selectors ----
    pub fn span_name(mut self, sub: &str) -> Self {
        self.qualifiers.push(format!("sn={}", esc(sub)));
        self
    }
    pub fn span_name_regex(mut self, r: &str, ci: bool) -> Self {
        self.qualifiers.push(format!("sn={}", regex_lit(r, ci)));
        self
    }
    pub fn service(mut self, sub: &str) -> Self {
        self.qualifiers.push(format!("sv={}", esc(sub)));
        self
    }
    pub fn service_regex(mut self, r: &str, ci: bool) -> Self {
        self.qualifiers.push(format!("sv={}", regex_lit(r, ci)));
        self
    }
    pub fn span_status(mut self, s: FilterSpanStatus) -> Self {
        self.qualifiers.push(format!("st={}", s.as_str()));
        self
    }
    pub fn span_kind(mut self, k: FilterSpanKind) -> Self {
        self.qualifiers.push(format!("sk={}", k.as_str()));
        self
    }
    pub fn duration_at_least_ms(mut self, ms: u32) -> Self {
        self.qualifiers.push(format!("d>={ms}"));
        self
    }
    pub fn duration_at_most_ms(mut self, ms: u32) -> Self {
        self.qualifiers.push(format!("d<={ms}"));
        self
    }

    // ---- Bookmarks ----
    pub fn bookmark_after(mut self, name: &str) -> Self {
        self.qualifiers.push(format!("b>={name}"));
        self
    }
    pub fn bookmark_before(mut self, name: &str) -> Self {
        self.qualifiers.push(format!("b<={name}"));
        self
    }

    // ---- Escape hatch for additional fields ----
    pub fn additional_field(mut self, name: &str, value: &str) -> Self {
        self.qualifiers.push(format!("{name}={}", esc(value)));
        self
    }
    pub fn additional_field_regex(mut self, name: &str, regex: &str, ci: bool) -> Self {
        self.qualifiers
            .push(format!("{name}={}", regex_lit(regex, ci)));
        self
    }
}
