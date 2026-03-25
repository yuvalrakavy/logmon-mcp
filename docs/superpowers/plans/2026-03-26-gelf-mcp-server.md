# GELF MCP Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Rust MCP server that receives GELF logs over UDP/TCP and exposes them to Claude via tools and notifications, with a filter DSL, trigger system, and event capture windows.

**Architecture:** Single Tokio async binary. GELF listeners feed logs through a pipeline: pre-trigger buffer + trigger engine + buffer filters + LogStore. The MCP server (stdio transport via `rmcp`) exposes tools for querying, filtering, and trigger management. Notifications alert Claude when triggers fire.

**Tech Stack:** Rust, Tokio, rmcp (MCP SDK v1.2), serde/serde_json, clap (CLI), regex

**Spec:** `docs/superpowers/specs/2026-03-25-gelf-mcp-server-design.md`

---

## File Map

```
gelf-mcp/
├── Cargo.toml
├── src/
│   ├── main.rs                  # CLI parsing, Tokio runtime, wiring
│   ├── config.rs                # Config struct from CLI args + env vars
│   ├── gelf/
│   │   ├── mod.rs               # re-exports
│   │   ├── message.rs           # LogEntry, Level, LogSource, GELF parsing
│   │   ├── udp.rs               # UDP listener task
│   │   └── tcp.rs               # TCP listener task
│   ├── filter/
│   │   ├── mod.rs               # re-exports
│   │   ├── parser.rs            # DSL string → ParsedFilter
│   │   └── matcher.rs           # ParsedFilter evaluation against LogEntry
│   ├── store/
│   │   ├── mod.rs               # re-exports
│   │   ├── traits.rs            # LogStore trait, StoreStats, BufferFilter
│   │   └── memory.rs            # InMemoryStore implementation
│   ├── engine/
│   │   ├── mod.rs               # re-exports
│   │   ├── trigger.rs           # Trigger struct, TriggerManager
│   │   ├── pre_buffer.rs        # PreTriggerBuffer (ring buffer)
│   │   └── pipeline.rs          # LogPipeline: orchestrates the full flow
│   └── mcp/
│       ├── mod.rs               # re-exports
│       ├── server.rs            # GelfMcpServer struct, ServerHandler impl
│       ├── tools_logs.rs        # get_recent_logs, get_log_context, export_logs, clear_logs
│       ├── tools_filters.rs     # get_filters, add_filter, edit_filter, remove_filter
│       ├── tools_triggers.rs    # get_triggers, add_trigger, edit_trigger, remove_trigger
│       └── tools_status.rs      # get_status
├── tests/
│   ├── filter_dsl.rs            # DSL parser + matcher tests
│   ├── memory_store.rs          # InMemoryStore tests
│   ├── pre_buffer.rs            # PreTriggerBuffer tests
│   ├── pipeline.rs              # LogPipeline integration tests
│   ├── gelf_parsing.rs          # GELF message parsing tests
│   ├── gelf_udp.rs              # UDP listener integration test
│   ├── gelf_tcp.rs              # TCP listener integration test
│   └── mcp_tools.rs             # MCP tool handler tests
└── skill/
    └── gelf-logs.md             # Claude Code skill for using this MCP
```

---

## Task 1: Project Scaffold

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`

- [ ] **Step 1: Initialize Cargo project**

```bash
cd /Users/yuval/Documents/Projects/MCPs/gelf-mcp
cargo init --name gelf-mcp-server
```

- [ ] **Step 2: Set up Cargo.toml with dependencies**

```toml
[package]
name = "gelf-mcp-server"
version = "0.1.0"
edition = "2021"
description = "MCP server that collects GELF logs and exposes them to Claude"

[dependencies]
rmcp = { version = "1.2", features = ["server", "transport-io"] }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = "0.8"
chrono = { version = "0.4", features = ["serde"] }
regex = "1"
clap = { version = "4", features = ["derive", "env"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
anyhow = "1"
thiserror = "2"

[dev-dependencies]
tokio-test = "0.4"
```

- [ ] **Step 3: Minimal main.rs that compiles**

```rust
fn main() {
    println!("gelf-mcp-server");
}
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build`
Expected: Compiles successfully

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/main.rs
git commit -m "feat: project scaffold with dependencies"
```

---

## Task 2: Data Model — LogEntry, Level, LogSource

**Files:**
- Create: `src/gelf/mod.rs`
- Create: `src/gelf/message.rs`
- Create: `tests/gelf_parsing.rs`

- [ ] **Step 1: Write tests for GELF level mapping and LogEntry parsing**

`tests/gelf_parsing.rs`:
```rust
use gelf_mcp_server::gelf::message::{LogEntry, Level, LogSource, parse_gelf_message};
use serde_json::json;

#[test]
fn test_level_from_syslog() {
    assert_eq!(Level::from_syslog(0), Level::Error);
    assert_eq!(Level::from_syslog(3), Level::Error);
    assert_eq!(Level::from_syslog(4), Level::Warn);
    assert_eq!(Level::from_syslog(5), Level::Info);
    assert_eq!(Level::from_syslog(6), Level::Info);
    assert_eq!(Level::from_syslog(7), Level::Debug);
}

#[test]
fn test_level_severity_ordering() {
    assert!(Level::Error > Level::Warn);
    assert!(Level::Warn > Level::Info);
    assert!(Level::Info > Level::Debug);
    assert!(Level::Debug > Level::Trace);
}

#[test]
fn test_parse_minimal_gelf() {
    let raw = json!({
        "version": "1.1",
        "host": "myapp",
        "short_message": "something happened",
        "level": 4
    });
    let entry = parse_gelf_message(raw.to_string().as_bytes(), 1).unwrap();
    assert_eq!(entry.host, "myapp");
    assert_eq!(entry.message, "something happened");
    assert_eq!(entry.level, Level::Warn);
    assert_eq!(entry.seq, 1);
}

#[test]
fn test_parse_full_gelf() {
    let raw = json!({
        "version": "1.1",
        "host": "myapp",
        "short_message": "timeout",
        "full_message": "stack trace here",
        "level": 3,
        "facility": "myapp::network",
        "file": "network.rs",
        "line": 42,
        "timestamp": 1700000000.123,
        "_request_id": "abc-123",
        "_user": "admin"
    });
    let entry = parse_gelf_message(raw.to_string().as_bytes(), 5).unwrap();
    assert_eq!(entry.level, Level::Error);
    assert_eq!(entry.full_message.as_deref(), Some("stack trace here"));
    assert_eq!(entry.facility.as_deref(), Some("myapp::network"));
    assert_eq!(entry.file.as_deref(), Some("network.rs"));
    assert_eq!(entry.line, Some(42));
    assert_eq!(entry.additional_fields.get("request_id").unwrap(), "abc-123");
    assert_eq!(entry.additional_fields.get("user").unwrap(), "admin");
}

#[test]
fn test_parse_gelf_missing_required_fields() {
    let raw = json!({"version": "1.1"});
    assert!(parse_gelf_message(raw.to_string().as_bytes(), 1).is_err());
}

#[test]
fn test_parse_gelf_invalid_json() {
    assert!(parse_gelf_message(b"not json", 1).is_err());
}

#[test]
fn test_trace_level_from_additional_field() {
    let raw = json!({
        "version": "1.1",
        "host": "myapp",
        "short_message": "trace msg",
        "level": 7,
        "_level": "TRACE"
    });
    let entry = parse_gelf_message(raw.to_string().as_bytes(), 1).unwrap();
    assert_eq!(entry.level, Level::Trace);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test gelf_parsing`
Expected: FAIL — module doesn't exist

- [ ] **Step 3: Implement LogEntry, Level, LogSource, parse_gelf_message**

`src/gelf/message.rs`:
```rust
use chrono::{DateTime, Utc, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Level {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl Level {
    pub fn from_syslog(level: u8) -> Self {
        match level {
            0..=3 => Level::Error,
            4 => Level::Warn,
            5 | 6 => Level::Info,
            _ => Level::Debug,
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_uppercase().as_str() {
            "ERROR" => Some(Level::Error),
            "WARN" | "WARNING" => Some(Level::Warn),
            "INFO" => Some(Level::Info),
            "DEBUG" => Some(Level::Debug),
            "TRACE" => Some(Level::Trace),
            _ => None,
        }
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Level::Error => write!(f, "ERROR"),
            Level::Warn => write!(f, "WARN"),
            Level::Info => write!(f, "INFO"),
            Level::Debug => write!(f, "DEBUG"),
            Level::Trace => write!(f, "TRACE"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogSource {
    Filter,
    PreTrigger,
    PostTrigger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    pub level: Level,
    pub message: String,
    pub full_message: Option<String>,
    pub host: String,
    pub facility: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub additional_fields: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub matched_filters: Vec<String>,
    #[serde(default = "default_source")]
    pub source: LogSource,
}

fn default_source() -> LogSource {
    LogSource::Filter
}

#[derive(Debug, Error)]
pub enum GelfParseError {
    #[error("invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("missing required field: {0}")]
    MissingField(&'static str),
}

pub fn parse_gelf_message(bytes: &[u8], seq: u64) -> Result<LogEntry, GelfParseError> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let obj = v.as_object().ok_or(GelfParseError::MissingField("root object"))?;

    let host = obj.get("host")
        .and_then(|v| v.as_str())
        .ok_or(GelfParseError::MissingField("host"))?
        .to_string();

    let message = obj.get("short_message")
        .and_then(|v| v.as_str())
        .ok_or(GelfParseError::MissingField("short_message"))?
        .to_string();

    let syslog_level = obj.get("level")
        .and_then(|v| v.as_u64())
        .unwrap_or(6) as u8;

    // Check for _level additional field (tracing-gelf sets this for TRACE)
    let level = obj.get("_level")
        .and_then(|v| v.as_str())
        .and_then(Level::from_name)
        .unwrap_or_else(|| Level::from_syslog(syslog_level));

    let timestamp = obj.get("timestamp")
        .and_then(|v| v.as_f64())
        .map(|ts| {
            let secs = ts as i64;
            let nanos = ((ts - secs as f64) * 1_000_000_000.0) as u32;
            Utc.timestamp_opt(secs, nanos).single().unwrap_or_else(Utc::now)
        })
        .unwrap_or_else(Utc::now);

    let full_message = obj.get("full_message").and_then(|v| v.as_str()).map(String::from);
    let facility = obj.get("facility").and_then(|v| v.as_str()).map(String::from);
    let file = obj.get("file").and_then(|v| v.as_str()).map(String::from);
    let line = obj.get("line").and_then(|v| v.as_u64()).map(|v| v as u32);

    // Collect additional fields (keys starting with _)
    let additional_fields: HashMap<String, serde_json::Value> = obj.iter()
        .filter(|(k, _)| k.starts_with('_') && *k != "_level")
        .map(|(k, v)| (k[1..].to_string(), v.clone()))
        .collect();

    Ok(LogEntry {
        seq,
        timestamp,
        level,
        message,
        full_message,
        host,
        facility,
        file,
        line,
        additional_fields,
        matched_filters: Vec::new(),
        source: LogSource::Filter,
    })
}
```

`src/gelf/mod.rs`:
```rust
pub mod message;
```

Add to `src/main.rs`:
```rust
pub mod gelf;

fn main() {
    println!("gelf-mcp-server");
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test gelf_parsing`
Expected: All 7 tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/gelf/ tests/gelf_parsing.rs src/main.rs
git commit -m "feat: LogEntry data model with GELF parsing"
```

---

## Task 3: Filter DSL — Parser

**Files:**
- Create: `src/filter/mod.rs`
- Create: `src/filter/parser.rs`
- Create: `tests/filter_dsl.rs`

- [ ] **Step 1: Write parser tests**

`tests/filter_dsl.rs`:
```rust
use gelf_mcp_server::filter::parser::*;

#[test]
fn test_parse_all() {
    let f = parse_filter("ALL").unwrap();
    assert!(matches!(f, ParsedFilter::All));
}

#[test]
fn test_parse_none() {
    let f = parse_filter("NONE").unwrap();
    assert!(matches!(f, ParsedFilter::None));
}

#[test]
fn test_parse_bare_substring() {
    let f = parse_filter("bug").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 1);
        assert!(matches!(&qs[0], Qualifier::BarePattern(Pattern::Substring(s)) if s == "bug"));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_bare_regex() {
    let f = parse_filter("/^ERROR.*timeout/").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 1);
        assert!(matches!(&qs[0], Qualifier::BarePattern(Pattern::Regex { case_insensitive: false, .. })));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_regex_case_insensitive() {
    let f = parse_filter("/pattern/i").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::BarePattern(Pattern::Regex { case_insensitive: true, .. })));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_selector_pattern() {
    let f = parse_filter("h=myapp").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::Host, Pattern::Substring(s)) if s == "myapp"));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_level_selector() {
    let f = parse_filter("l>=WARN").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::LevelFilter { .. }));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_multiple_qualifiers() {
    let f = parse_filter("bug,h=myapp,l>=ERROR").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 3);
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_quoted_pattern_with_comma() {
    let f = parse_filter("\"connection refused, retrying\"").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 1);
        assert!(matches!(&qs[0], Qualifier::BarePattern(Pattern::Substring(s)) if s == "connection refused, retrying"));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_quoted_pattern_with_equals() {
    let f = parse_filter("\"key=value\"").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 1);
        assert!(matches!(&qs[0], Qualifier::BarePattern(Pattern::Substring(s)) if s == "key=value"));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_mfm_selector() {
    let f = parse_filter("mfm=timeout").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::MessageOrFull, _)));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_custom_additional_field() {
    let f = parse_filter("request_id=abc").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::AdditionalField(name), _) if name == "request_id"));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_empty_string_error() {
    assert!(parse_filter("").is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test filter_dsl`
Expected: FAIL — module doesn't exist

- [ ] **Step 3: Implement the parser**

`src/filter/parser.rs` — Implement the following types and `parse_filter` function:

```rust
use crate::gelf::message::Level;
use regex::Regex;
use thiserror::Error;
use serde::{Serialize, Deserialize};

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
    LevelFilter {
        op: LevelOp,
        level: Level,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Pattern {
    Substring(String),  // case-insensitive, stored lowercase
    #[serde(skip)]
    Regex {
        source: String,
        #[serde(skip)]
        compiled: Regex,
        case_insensitive: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Selector {
    Message,
    FullMessage,
    MessageOrFull,
    Host,
    Facility,
    File,
    Line,
    AdditionalField(String),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LevelOp { Eq, Gte, Lte }

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
}

pub fn parse_filter(input: &str) -> Result<ParsedFilter, FilterParseError> {
    // Implementation: split on commas (respecting quotes), parse each qualifier
    // See spec for full DSL grammar
    todo!()
}
```

Key implementation details:
- Split input on `,` but respect double-quoted strings
- For each token: check if it's `ALL`/`NONE`, starts with `l>=`/`l<=`/`l=`, contains `=` (selector), starts with `/` (regex), or is a bare substring
- Known selectors: `m`, `fm`, `mfm`, `h`, `fa`, `fi`, `ln` — anything else is `AdditionalField`
- Substrings stored lowercase for case-insensitive matching
- Regex compiled with `(?i)` flag if `/i` suffix present

`src/filter/mod.rs`:
```rust
pub mod parser;
pub mod matcher;
```

Add `pub mod filter;` to `src/main.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test filter_dsl`
Expected: All 13 tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/filter/ tests/filter_dsl.rs src/main.rs
git commit -m "feat: filter DSL parser"
```

---

## Task 4: Filter DSL — Matcher

**Files:**
- Create: `src/filter/matcher.rs`
- Modify: `tests/filter_dsl.rs` (add matcher tests)

- [ ] **Step 1: Write matcher tests**

Append to `tests/filter_dsl.rs`:
```rust
use gelf_mcp_server::filter::matcher::matches_entry;
use gelf_mcp_server::gelf::message::{LogEntry, Level, LogSource};
use chrono::Utc;
use std::collections::HashMap;

fn test_entry(level: Level, message: &str, host: &str) -> LogEntry {
    LogEntry {
        seq: 1,
        timestamp: Utc::now(),
        level,
        message: message.to_string(),
        full_message: None,
        host: host.to_string(),
        facility: Some("test::module".to_string()),
        file: Some("test.rs".to_string()),
        line: Some(42),
        additional_fields: HashMap::new(),
        matched_filters: Vec::new(),
        source: LogSource::Filter,
    }
}

#[test]
fn test_match_all() {
    let f = parse_filter("ALL").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Debug, "hello", "app")));
}

#[test]
fn test_match_none() {
    let f = parse_filter("NONE").unwrap();
    assert!(!matches_entry(&f, &test_entry(Level::Debug, "hello", "app")));
}

#[test]
fn test_match_substring_case_insensitive() {
    let f = parse_filter("BUG").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Info, "found a bug here", "app")));
    assert!(matches_entry(&f, &test_entry(Level::Info, "BUG report", "app")));
    assert!(!matches_entry(&f, &test_entry(Level::Info, "all good", "app")));
}

#[test]
fn test_match_host_selector() {
    let f = parse_filter("h=myapp").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Info, "hello", "myapp")));
    assert!(!matches_entry(&f, &test_entry(Level::Info, "hello", "other")));
}

#[test]
fn test_match_level_gte() {
    let f = parse_filter("l>=WARN").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Error, "err", "app")));
    assert!(matches_entry(&f, &test_entry(Level::Warn, "warn", "app")));
    assert!(!matches_entry(&f, &test_entry(Level::Info, "info", "app")));
}

#[test]
fn test_match_combined_qualifiers() {
    let f = parse_filter("timeout,h=myapp,l>=WARN").unwrap();
    // All three must match (AND)
    assert!(matches_entry(&f, &test_entry(Level::Error, "connection timeout", "myapp")));
    assert!(!matches_entry(&f, &test_entry(Level::Error, "connection timeout", "other")));
    assert!(!matches_entry(&f, &test_entry(Level::Info, "connection timeout", "myapp")));
    assert!(!matches_entry(&f, &test_entry(Level::Error, "success", "myapp")));
}

#[test]
fn test_match_facility_selector() {
    let f = parse_filter("fa=test").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Info, "msg", "app")));
}

#[test]
fn test_match_file_selector() {
    let f = parse_filter("fi=test.rs").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Info, "msg", "app")));
}

#[test]
fn test_match_regex() {
    let f = parse_filter("/^found.*bug/i").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Info, "Found a Bug here", "app")));
    assert!(!matches_entry(&f, &test_entry(Level::Info, "no match", "app")));
}

#[test]
fn test_bare_substring_matches_all_fields() {
    let f = parse_filter("myapp").unwrap();
    // Should match host field too
    assert!(matches_entry(&f, &test_entry(Level::Info, "something", "myapp")));
}
```

- [ ] **Step 2: Run tests to verify new tests fail**

Run: `cargo test --test filter_dsl`
Expected: New matcher tests FAIL

- [ ] **Step 3: Implement the matcher**

`src/filter/matcher.rs`:
```rust
use crate::filter::parser::*;
use crate::gelf::message::LogEntry;

pub fn matches_entry(filter: &ParsedFilter, entry: &LogEntry) -> bool {
    match filter {
        ParsedFilter::All => true,
        ParsedFilter::None => false,
        ParsedFilter::Qualifiers(qs) => qs.iter().all(|q| matches_qualifier(q, entry)),
    }
}

fn matches_qualifier(qualifier: &Qualifier, entry: &LogEntry) -> bool {
    // For BarePattern: check against all searchable fields (message, full_message, host, facility, file, line, additional_fields)
    // For SelectorPattern: check against the specific field(s)
    // For LevelFilter: compare entry.level with the filter level using the operator
    todo!()
}
```

Key implementation details:
- Bare patterns search across: message, full_message, host, facility, file, line (as string), additional field values (as string)
- Substring matching: `entry_field.to_lowercase().contains(&pattern)` (pattern already stored lowercase)
- `mfm` selector: match if message OR full_message matches
- `ln` selector: convert line number to string for matching
- Level comparison uses the `Ord` impl on `Level`

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test filter_dsl`
Expected: All tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/filter/matcher.rs tests/filter_dsl.rs
git commit -m "feat: filter DSL matcher"
```

---

## Task 5: LogStore Trait + InMemoryStore

**Files:**
- Create: `src/store/mod.rs`
- Create: `src/store/traits.rs`
- Create: `src/store/memory.rs`
- Create: `tests/memory_store.rs`

- [ ] **Step 1: Write InMemoryStore tests**

`tests/memory_store.rs`:
```rust
use gelf_mcp_server::store::traits::*;
use gelf_mcp_server::store::memory::InMemoryStore;
use gelf_mcp_server::gelf::message::{LogEntry, Level, LogSource};
use chrono::Utc;
use std::collections::HashMap;

fn make_entry(seq: u64, level: Level, msg: &str) -> LogEntry {
    LogEntry {
        seq, timestamp: Utc::now(), level,
        message: msg.to_string(), full_message: None,
        host: "test".into(), facility: None, file: None, line: None,
        additional_fields: HashMap::new(),
        matched_filters: Vec::new(), source: LogSource::Filter,
    }
}

#[test]
fn test_append_and_recent() {
    let store = InMemoryStore::new(100);
    store.append(make_entry(1, Level::Info, "first"));
    store.append(make_entry(2, Level::Info, "second"));
    let recent = store.recent(10, None);
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].seq, 2); // newest first
    assert_eq!(recent[1].seq, 1);
}

#[test]
fn test_max_capacity() {
    let store = InMemoryStore::new(3);
    for i in 1..=5 {
        store.append(make_entry(i, Level::Info, &format!("msg {i}")));
    }
    assert_eq!(store.len(), 3);
    let recent = store.recent(10, None);
    assert_eq!(recent[0].seq, 5);
    assert_eq!(recent[2].seq, 3); // oldest surviving
}

#[test]
fn test_contains_seq() {
    let store = InMemoryStore::new(100);
    store.append(make_entry(42, Level::Info, "hello"));
    assert!(store.contains_seq(42));
    assert!(!store.contains_seq(99));
}

#[test]
fn test_context_by_seq() {
    let store = InMemoryStore::new(100);
    for i in 1..=10 {
        store.append(make_entry(i, Level::Info, &format!("msg {i}")));
    }
    let ctx = store.context_by_seq(5, 2, 2);
    assert_eq!(ctx.len(), 5); // seq 3,4,5,6,7
    assert_eq!(ctx[0].seq, 3);
    assert_eq!(ctx[4].seq, 7);
}

#[test]
fn test_clear() {
    let store = InMemoryStore::new(100);
    store.append(make_entry(1, Level::Info, "hello"));
    assert_eq!(store.len(), 1);
    store.clear();
    assert_eq!(store.len(), 0);
}

#[test]
fn test_recent_with_filter() {
    let store = InMemoryStore::new(100);
    store.append(make_entry(1, Level::Info, "good"));
    store.append(make_entry(2, Level::Error, "bad"));
    store.append(make_entry(3, Level::Info, "good again"));

    use gelf_mcp_server::filter::parser::parse_filter;
    let filter = parse_filter("l>=ERROR").unwrap();
    let recent = store.recent(10, Some(&filter));
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].seq, 2);
}

#[test]
fn test_stats() {
    let store = InMemoryStore::new(100);
    store.append(make_entry(1, Level::Info, "hello"));
    store.append(make_entry(2, Level::Info, "world"));
    let stats = store.stats();
    assert_eq!(stats.total_stored, 2);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test memory_store`
Expected: FAIL

- [ ] **Step 3: Implement LogStore trait and InMemoryStore**

`src/store/traits.rs` — Define `LogStore` trait, `StoreStats` struct.
`src/store/memory.rs` — `InMemoryStore` with `VecDeque<LogEntry>` behind `RwLock`.

Key details:
- `recent()` returns entries newest-first, applying optional filter via `matches_entry`
- `context_by_seq()` finds the entry with the given seq, then returns `before` entries before + the entry + `after` entries after
- `contains_seq()` uses a `HashSet<u64>` alongside the `VecDeque` for O(1) lookup
- When evicting from ring buffer, also remove from HashSet
- `StoreStats` tracks `total_received`, `total_stored`, `malformed_count` via `AtomicU64`

Add `pub mod store;` to `src/main.rs`.

- [ ] **Step 4: Run tests**

Run: `cargo test --test memory_store`
Expected: All 8 tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/store/ tests/memory_store.rs src/main.rs
git commit -m "feat: LogStore trait and InMemoryStore"
```

---

## Task 6: Pre-trigger Buffer

**Files:**
- Create: `src/engine/mod.rs`
- Create: `src/engine/pre_buffer.rs`
- Create: `tests/pre_buffer.rs`

- [ ] **Step 1: Write pre-trigger buffer tests**

`tests/pre_buffer.rs`:
```rust
use gelf_mcp_server::engine::pre_buffer::PreTriggerBuffer;
use gelf_mcp_server::gelf::message::{LogEntry, Level, LogSource};
use chrono::Utc;
use std::collections::HashMap;

fn make_entry(seq: u64) -> LogEntry {
    LogEntry {
        seq, timestamp: Utc::now(), level: Level::Info,
        message: format!("msg {seq}"), full_message: None,
        host: "test".into(), facility: None, file: None, line: None,
        additional_fields: HashMap::new(),
        matched_filters: Vec::new(), source: LogSource::Filter,
    }
}

#[test]
fn test_append_and_flush() {
    let buf = PreTriggerBuffer::new(5);
    for i in 1..=3 {
        buf.append(make_entry(i));
    }
    let flushed = buf.flush(3);
    assert_eq!(flushed.len(), 3);
    assert_eq!(flushed[0].seq, 1);
    assert_eq!(flushed[2].seq, 3);
}

#[test]
fn test_flush_respects_pre_window() {
    let buf = PreTriggerBuffer::new(10);
    for i in 1..=10 {
        buf.append(make_entry(i));
    }
    let flushed = buf.flush(3); // only last 3
    assert_eq!(flushed.len(), 3);
    assert_eq!(flushed[0].seq, 8);
    assert_eq!(flushed[2].seq, 10);
}

#[test]
fn test_ring_buffer_eviction() {
    let buf = PreTriggerBuffer::new(3);
    for i in 1..=5 {
        buf.append(make_entry(i));
    }
    let flushed = buf.flush(10); // ask for more than capacity
    assert_eq!(flushed.len(), 3);
    assert_eq!(flushed[0].seq, 3); // oldest surviving
}

#[test]
fn test_flush_removes_only_flushed_entries() {
    let buf = PreTriggerBuffer::new(10);
    for i in 1..=10 {
        buf.append(make_entry(i));
    }
    let flushed = buf.flush(3); // flush last 3 (8, 9, 10)
    assert_eq!(flushed.len(), 3);
    // Remaining entries should still be there
    let flushed2 = buf.flush(10);
    assert_eq!(flushed2.len(), 7); // entries 1-7
}

#[test]
fn test_resize() {
    let buf = PreTriggerBuffer::new(3);
    buf.resize(5);
    for i in 1..=5 {
        buf.append(make_entry(i));
    }
    assert_eq!(buf.flush(10).len(), 5);
}

#[test]
fn test_disabled_when_zero() {
    let buf = PreTriggerBuffer::new(0);
    buf.append(make_entry(1));
    assert_eq!(buf.flush(10).len(), 0);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test pre_buffer`
Expected: FAIL

- [ ] **Step 3: Implement PreTriggerBuffer**

`src/engine/pre_buffer.rs`:
- `VecDeque<LogEntry>` behind `Mutex`
- `capacity: AtomicUsize`
- `append()`: push back, evict front if over capacity
- `flush(pre_window)`: drain the last `min(pre_window, len)` entries, return them in chronological order
- `resize()`: update capacity, evict if needed

`src/engine/mod.rs`:
```rust
pub mod pre_buffer;
pub mod trigger;
pub mod pipeline;
```

Add `pub mod engine;` to `src/main.rs`.

- [ ] **Step 4: Run tests**

Run: `cargo test --test pre_buffer`
Expected: All 6 tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/engine/ tests/pre_buffer.rs src/main.rs
git commit -m "feat: pre-trigger buffer (flight recorder)"
```

---

## Task 7: Trigger Types + Manager

**Files:**
- Create: `src/engine/trigger.rs`

- [ ] **Step 1: Write trigger tests**

Add to `tests/pipeline.rs` (created in next task, but trigger unit tests here):

```rust
// tests at the end of src/engine/trigger.rs as unit tests
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_triggers() {
        let mgr = TriggerManager::new();
        let triggers = mgr.list();
        assert_eq!(triggers.len(), 2);
        assert_eq!(triggers[0].id, 1);
        assert_eq!(triggers[1].id, 2);
    }

    #[test]
    fn test_add_trigger() {
        let mgr = TriggerManager::new();
        let id = mgr.add("fa=mqtt", 500, 200, 5, Some("MQTT watch")).unwrap();
        assert_eq!(id, 3);
        assert_eq!(mgr.list().len(), 3);
    }

    #[test]
    fn test_remove_trigger() {
        let mgr = TriggerManager::new();
        mgr.remove(1).unwrap();
        assert_eq!(mgr.list().len(), 1);
    }

    #[test]
    fn test_edit_trigger() {
        let mgr = TriggerManager::new();
        mgr.edit(1, Some("l>=WARN"), Some(100), Some(50), Some(3), Some("edited")).unwrap();
        let t = mgr.get(1).unwrap();
        assert_eq!(t.pre_window, 100);
        assert_eq!(t.post_window, 50);
    }

    #[test]
    fn test_max_pre_window() {
        let mgr = TriggerManager::new();
        mgr.add("fa=test", 1000, 200, 5, None).unwrap();
        assert_eq!(mgr.max_pre_window(), 1000);
    }
}
```

- [ ] **Step 2: Implement TriggerManager**

`src/engine/trigger.rs`:
```rust
use crate::filter::parser::{ParsedFilter, parse_filter, FilterParseError};
use std::sync::{RwLock, atomic::{AtomicU32, AtomicU64, Ordering}};

pub struct Trigger {
    pub id: u32,
    pub condition: ParsedFilter,
    pub pre_window: u32,
    pub post_window: u32,
    pub notify_context: u32,
    pub description: Option<String>,
    pub match_count: AtomicU64,
}

pub struct TriggerManager {
    triggers: RwLock<Vec<Trigger>>,
    next_id: AtomicU32,
}

impl TriggerManager {
    pub fn new() -> Self { /* create with 2 default triggers */ }
    pub fn list(&self) -> Vec<TriggerInfo> { /* snapshot */ }
    pub fn get(&self, id: u32) -> Option<TriggerInfo> { }
    pub fn add(&self, filter: &str, pre: u32, post: u32, ctx: u32, desc: Option<&str>) -> Result<u32, FilterParseError> { }
    pub fn edit(&self, id: u32, filter: Option<&str>, pre: Option<u32>, post: Option<u32>, ctx: Option<u32>, desc: Option<&str>) -> Result<TriggerInfo, Error> { }
    pub fn remove(&self, id: u32) -> Result<(), Error> { }
    pub fn max_pre_window(&self) -> u32 { }
    pub fn evaluate(&self, entry: &LogEntry) -> Vec<TriggerMatch> { }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test --lib engine::trigger::tests`
Expected: All tests PASS

- [ ] **Step 4: Commit**

```bash
git add src/engine/trigger.rs
git commit -m "feat: trigger types and TriggerManager"
```

---

## Task 8: Log Pipeline (Orchestrator)

**Files:**
- Create: `src/engine/pipeline.rs`
- Create: `tests/pipeline.rs`

- [ ] **Step 1: Write pipeline integration tests**

`tests/pipeline.rs`:
```rust
use gelf_mcp_server::engine::pipeline::{LogPipeline, PipelineEvent};
use gelf_mcp_server::gelf::message::{LogEntry, Level, LogSource};
use chrono::Utc;
use std::collections::HashMap;

fn make_entry(seq: u64, level: Level, msg: &str) -> LogEntry {
    LogEntry {
        seq, timestamp: Utc::now(), level,
        message: msg.to_string(), full_message: None,
        host: "test".into(), facility: None, file: None, line: None,
        additional_fields: HashMap::new(),
        matched_filters: Vec::new(), source: LogSource::Filter,
    }
}

#[test]
fn test_normal_flow_no_filters_stores_everything() {
    let pipeline = LogPipeline::new(1000);
    let events = pipeline.process(make_entry(1, Level::Info, "hello"));
    assert_eq!(pipeline.store().len(), 1);
    assert!(events.is_empty()); // no trigger fired
}

#[test]
fn test_buffer_filter_blocks_non_matching() {
    let pipeline = LogPipeline::new(1000);
    pipeline.add_filter("l>=ERROR", Some("errors only")).unwrap();
    pipeline.process(make_entry(1, Level::Info, "hello"));
    assert_eq!(pipeline.store().len(), 0); // filtered out
    pipeline.process(make_entry(2, Level::Error, "bad"));
    assert_eq!(pipeline.store().len(), 1);
}

#[test]
fn test_trigger_fires_and_flushes_pre_buffer() {
    let pipeline = LogPipeline::new(1000);
    // Add filter that blocks everything except errors
    pipeline.add_filter("l>=ERROR", None).unwrap();

    // Send 10 info messages (won't be stored due to filter, but go to pre-buffer)
    for i in 1..=10 {
        pipeline.process(make_entry(i, Level::Info, &format!("info {i}")));
    }
    assert_eq!(pipeline.store().len(), 0);

    // Send an error — trigger fires, pre-buffer flushes
    let events = pipeline.process(make_entry(11, Level::Error, "crash!"));
    assert!(!events.is_empty());
    // Pre-buffer entries + the error itself should be in store
    assert!(pipeline.store().len() > 1);
}

#[test]
fn test_post_window_bypasses_filters_and_triggers() {
    let pipeline = LogPipeline::new(1000);
    pipeline.add_filter("l>=ERROR", None).unwrap();

    // Trigger fires on error
    pipeline.process(make_entry(1, Level::Error, "crash"));
    let before = pipeline.store().len();

    // Next entries during post-window should be stored despite filter
    pipeline.process(make_entry(2, Level::Info, "recovery step 1"));
    assert_eq!(pipeline.store().len(), before + 1);
}

#[test]
fn test_post_window_expires() {
    let pipeline = LogPipeline::new(1000);
    pipeline.add_filter("l>=ERROR", None).unwrap();

    // Edit default trigger to have post_window=2 for easy testing
    pipeline.edit_trigger(1, None, None, Some(2), None, None).unwrap();

    pipeline.process(make_entry(1, Level::Error, "crash"));
    let after_trigger = pipeline.store().len();

    // 2 post-window entries
    pipeline.process(make_entry(2, Level::Info, "post 1"));
    pipeline.process(make_entry(3, Level::Info, "post 2"));

    // 3rd should be filtered normally
    pipeline.process(make_entry(4, Level::Info, "filtered out"));
    assert_eq!(pipeline.store().len(), after_trigger + 2);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test pipeline`
Expected: FAIL

- [ ] **Step 3: Implement LogPipeline**

`src/engine/pipeline.rs`:
```rust
use crate::engine::pre_buffer::PreTriggerBuffer;
use crate::engine::trigger::TriggerManager;
use crate::filter::matcher::matches_entry;
use crate::filter::parser::{ParsedFilter, parse_filter};
use crate::gelf::message::{LogEntry, LogSource};
use crate::store::memory::InMemoryStore;
use std::sync::{RwLock, atomic::{AtomicU32, Ordering}};

pub struct PipelineEvent {
    pub trigger_id: u32,
    pub trigger_description: Option<String>,
    pub filter_string: String,
    pub matched_entry: LogEntry,
    pub context_before: Vec<LogEntry>,
    pub pre_trigger_flushed: usize,
    pub post_window_size: u32,
}

pub struct BufferFilterEntry {
    pub id: u32,
    pub condition: ParsedFilter,
    pub description: Option<String>,
}

pub struct LogPipeline {
    store: InMemoryStore,
    pre_buffer: PreTriggerBuffer,
    triggers: TriggerManager,
    filters: RwLock<Vec<BufferFilterEntry>>,
    post_window_remaining: AtomicU32,
    next_filter_id: AtomicU32,
    seq_counter: AtomicU64,
}

impl LogPipeline {
    pub fn new(buffer_size: usize) -> Self { }
    pub fn process(&self, mut entry: LogEntry) -> Vec<PipelineEvent> {
        // Implements the trigger evaluation flow from spec:
        // 1. If post-window active: store directly, append to pre-buffer, skip triggers/filters
        // 2. Otherwise: append to pre-buffer, evaluate triggers, evaluate filters
    }
    pub fn store(&self) -> &InMemoryStore { }
    pub fn add_filter(&self, filter: &str, desc: Option<&str>) -> Result<u32, _> { }
    pub fn edit_filter(&self, id: u32, filter: Option<&str>, desc: Option<&str>) -> Result<_, _> { }
    pub fn remove_filter(&self, id: u32) -> Result<(), _> { }
    pub fn list_filters(&self) -> Vec<FilterInfo> { }
    // Delegate trigger methods to TriggerManager
    pub fn add_trigger(&self, ...) { }
    pub fn edit_trigger(&self, ...) { }
    pub fn remove_trigger(&self, ...) { }
    pub fn list_triggers(&self) -> Vec<TriggerInfo> { }
    pub fn assign_seq(&self) -> u64 { }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test pipeline`
Expected: All 5 tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/engine/pipeline.rs tests/pipeline.rs
git commit -m "feat: log pipeline orchestrator"
```

---

## Task 9: GELF UDP Listener

**Files:**
- Create: `src/gelf/udp.rs`
- Create: `tests/gelf_udp.rs`

- [ ] **Step 1: Write UDP listener integration test**

`tests/gelf_udp.rs`:
```rust
use std::net::UdpSocket;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use serde_json::json;

#[tokio::test]
async fn test_udp_listener_receives_gelf() {
    let pipeline = Arc::new(gelf_mcp_server::engine::pipeline::LogPipeline::new(1000));
    let addr = "127.0.0.1:0"; // OS picks a free port

    let listener = gelf_mcp_server::gelf::udp::start_udp_listener(addr, pipeline.clone()).await.unwrap();
    let port = listener.local_addr;

    // Send a GELF message via UDP
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let msg = json!({
        "version": "1.1",
        "host": "test",
        "short_message": "hello from udp",
        "level": 6
    });
    socket.send_to(msg.to_string().as_bytes(), format!("127.0.0.1:{port}")).unwrap();

    sleep(Duration::from_millis(100)).await;
    assert_eq!(pipeline.store().len(), 1);
    let logs = pipeline.store().recent(1, None);
    assert_eq!(logs[0].message, "hello from udp");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test gelf_udp`
Expected: FAIL

- [ ] **Step 3: Implement UDP listener**

`src/gelf/udp.rs`:
```rust
use crate::engine::pipeline::LogPipeline;
use crate::gelf::message::parse_gelf_message;
use std::sync::Arc;
use tokio::net::UdpSocket;

pub struct UdpListenerHandle {
    pub local_addr: u16,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

pub async fn start_udp_listener(
    addr: &str,
    pipeline: Arc<LogPipeline>,
) -> anyhow::Result<UdpListenerHandle> {
    let socket = UdpSocket::bind(addr).await?;
    let port = socket.local_addr()?.port();
    let (tx, mut rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let mut buf = [0u8; 65535]; // max UDP datagram
        loop {
            tokio::select! {
                result = socket.recv_from(&mut buf) => {
                    if let Ok((len, _addr)) = result {
                        let seq = pipeline.assign_seq();
                        match parse_gelf_message(&buf[..len], seq) {
                            Ok(entry) => { pipeline.process(entry); }
                            Err(e) => {
                                eprintln!("malformed GELF UDP: {e}");
                                pipeline.store().stats().increment_malformed();
                            }
                        }
                    }
                }
                _ = &mut rx => break,
            }
        }
    });

    Ok(UdpListenerHandle { local_addr: port, shutdown: tx })
}
```

- [ ] **Step 4: Run test**

Run: `cargo test --test gelf_udp`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/gelf/udp.rs tests/gelf_udp.rs
git commit -m "feat: UDP GELF listener"
```

---

## Task 10: GELF TCP Listener

**Files:**
- Create: `src/gelf/tcp.rs`
- Create: `tests/gelf_tcp.rs`

- [ ] **Step 1: Write TCP listener integration test**

`tests/gelf_tcp.rs`:
```rust
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::{sleep, Duration};
use serde_json::json;

#[tokio::test]
async fn test_tcp_listener_receives_gelf() {
    let pipeline = Arc::new(gelf_mcp_server::engine::pipeline::LogPipeline::new(1000));

    let listener = gelf_mcp_server::gelf::tcp::start_tcp_listener(
        "127.0.0.1:0", pipeline.clone()
    ).await.unwrap();
    let port = listener.local_addr;

    // Connect and send null-delimited GELF
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
    let msg1 = json!({"version":"1.1","host":"tcp-test","short_message":"msg1","level":6});
    let msg2 = json!({"version":"1.1","host":"tcp-test","short_message":"msg2","level":4});

    stream.write_all(msg1.to_string().as_bytes()).await.unwrap();
    stream.write_all(b"\0").await.unwrap(); // null delimiter
    stream.write_all(msg2.to_string().as_bytes()).await.unwrap();
    stream.write_all(b"\0").await.unwrap();

    sleep(Duration::from_millis(100)).await;
    assert_eq!(pipeline.store().len(), 2);
}

#[tokio::test]
async fn test_tcp_multiple_clients() {
    let pipeline = Arc::new(gelf_mcp_server::engine::pipeline::LogPipeline::new(1000));
    let listener = gelf_mcp_server::gelf::tcp::start_tcp_listener(
        "127.0.0.1:0", pipeline.clone()
    ).await.unwrap();
    let port = listener.local_addr;

    for i in 0..3 {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        let msg = json!({"version":"1.1","host":"client","short_message":format!("from {i}"),"level":6});
        stream.write_all(msg.to_string().as_bytes()).await.unwrap();
        stream.write_all(b"\0").await.unwrap();
    }

    sleep(Duration::from_millis(200)).await;
    assert_eq!(pipeline.store().len(), 3);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test gelf_tcp`
Expected: FAIL

- [ ] **Step 3: Implement TCP listener**

`src/gelf/tcp.rs`:
- `TcpListener::bind`, accept connections in a loop
- For each connection, spawn a task that reads null-byte delimited JSON
- Use `tokio::io::BufReader` and read until `\0` byte
- Parse each complete message and feed to pipeline
- Track connected clients count with `AtomicU32`

- [ ] **Step 4: Run tests**

Run: `cargo test --test gelf_tcp`
Expected: All tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/gelf/tcp.rs tests/gelf_tcp.rs
git commit -m "feat: TCP GELF listener"
```

---

## Task 11: MCP Server Scaffold + Status Tool

**Files:**
- Create: `src/mcp/mod.rs`
- Create: `src/mcp/server.rs`
- Create: `src/mcp/tools_status.rs`

- [ ] **Step 1: Implement MCP server struct with tool_router**

`src/mcp/server.rs`:
```rust
use crate::engine::pipeline::LogPipeline;
use rmcp::{ServerHandler, model::*, tool::ToolRouter};
use std::sync::Arc;

#[derive(Clone)]
pub struct GelfMcpServer {
    pipeline: Arc<LogPipeline>,
    tool_router: ToolRouter<Self>,
    // GELF listener metadata
    udp_port: u16,
    tcp_port: u16,
    start_time: std::time::Instant,
}

impl GelfMcpServer {
    pub fn new(pipeline: Arc<LogPipeline>, udp_port: u16, tcp_port: u16) -> Self {
        Self {
            pipeline,
            tool_router: Self::tool_router(),
            udp_port,
            tcp_port,
            start_time: std::time::Instant::now(),
        }
    }
}

#[rmcp::tool_handler]
impl ServerHandler for GelfMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
    }
}
```

- [ ] **Step 2: Implement get_status tool**

`src/mcp/tools_status.rs`:
```rust
use super::server::GelfMcpServer;
use rmcp::{tool, model::*};
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;

#[rmcp::tool_router]
impl GelfMcpServer {
    #[tool(description = "Get current server status including buffer sizes, trigger counts, and connection info")]
    async fn get_status(&self) -> Result<CallToolResult, McpError> {
        let stats = self.pipeline.store().stats();
        let status = serde_json::json!({
            "buffer_size": self.pipeline.store().len(),
            "filter_count": self.pipeline.list_filters().len(),
            "trigger_count": self.pipeline.list_triggers().len(),
            "udp_port": self.udp_port,
            "tcp_port": self.tcp_port,
            "uptime_secs": self.start_time.elapsed().as_secs(),
            "total_received": stats.total_received,
            "total_stored": stats.total_stored,
            "malformed_count": stats.malformed_count,
        });
        Ok(CallToolResult::success(vec![Content::text(status.to_string())]))
    }
}
```

Note: The `#[tool_router]` macro must be on a single `impl` block. Since we have tools across multiple files, we need to either put all tools in one `impl` block, or use a different organization. Check the rmcp docs — if only one `#[tool_router]` block is supported, consolidate all tools into `server.rs`. Otherwise, split per file.

If consolidation is needed, restructure: keep tool logic in separate functions in each `tools_*.rs` file, but the `#[tool]` annotations all go in one `#[tool_router] impl` block in `server.rs`.

- [ ] **Step 3: Verify it compiles**

Run: `cargo build`
Expected: Compiles

- [ ] **Step 4: Commit**

```bash
git add src/mcp/
git commit -m "feat: MCP server scaffold with get_status tool"
```

---

## Task 12: MCP Log Query Tools

**Files:**
- Modify: `src/mcp/server.rs` (add tools to the tool_router block)
- Create: `src/mcp/tools_logs.rs` (helper functions)
- Create: `tests/mcp_tools.rs`

- [ ] **Step 1: Write tests for log query tools**

Test the pipeline-level functions that tools will call (testing MCP transport is integration-level; here we test the logic).

`tests/mcp_tools.rs`:
```rust
use gelf_mcp_server::engine::pipeline::LogPipeline;
use gelf_mcp_server::gelf::message::{LogEntry, Level, LogSource};
use chrono::Utc;
use std::collections::HashMap;

fn make_entry(seq: u64, level: Level, msg: &str) -> LogEntry {
    LogEntry {
        seq, timestamp: Utc::now(), level,
        message: msg.to_string(), full_message: None,
        host: "test".into(), facility: None, file: None, line: None,
        additional_fields: HashMap::new(),
        matched_filters: Vec::new(), source: LogSource::Filter,
    }
}

#[test]
fn test_get_recent_with_filter() {
    let pipeline = LogPipeline::new(1000);
    pipeline.process(make_entry(1, Level::Info, "hello"));
    pipeline.process(make_entry(2, Level::Error, "crash"));
    pipeline.process(make_entry(3, Level::Info, "world"));

    let logs = pipeline.store().recent(10, None);
    assert_eq!(logs.len(), 3);

    use gelf_mcp_server::filter::parser::parse_filter;
    let filter = parse_filter("l>=ERROR").unwrap();
    let errors = pipeline.store().recent(10, Some(&filter));
    assert_eq!(errors.len(), 1);
}

#[test]
fn test_clear_logs() {
    let pipeline = LogPipeline::new(1000);
    pipeline.process(make_entry(1, Level::Info, "hello"));
    assert_eq!(pipeline.store().len(), 1);
    pipeline.store().clear();
    assert_eq!(pipeline.store().len(), 0);
}

#[test]
fn test_export_logs_json() {
    let pipeline = LogPipeline::new(1000);
    pipeline.process(make_entry(1, Level::Info, "export me"));

    let path = std::env::temp_dir().join("gelf_test_export.json");
    let count = gelf_mcp_server::mcp::tools_logs::export_to_file(
        pipeline.store(), &path, None, None, "json"
    ).unwrap();
    assert_eq!(count, 1);

    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("export me"));
    std::fs::remove_file(&path).ok();
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test mcp_tools`
Expected: FAIL

- [ ] **Step 3: Implement log query tools**

Add `get_recent_logs`, `get_log_context`, `export_logs`, `clear_logs` as `#[tool]` methods. Implement `export_to_file` helper in `tools_logs.rs`.

Key tool parameter structs (derive `Deserialize`, `JsonSchema`):
```rust
#[derive(Deserialize, JsonSchema)]
pub struct GetRecentLogsParams {
    #[serde(default = "default_count")]
    pub count: u32,
    pub filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetLogContextParams {
    pub seq: Option<u64>,
    pub timestamp: Option<String>,
    #[serde(default = "default_10")]
    pub before: u32,
    #[serde(default = "default_10")]
    pub after: u32,
    #[serde(default = "default_5")]
    pub window_secs: u32,
}

#[derive(Deserialize, JsonSchema)]
pub struct ExportLogsParams {
    pub path: String,
    pub count: Option<u32>,
    pub filter: Option<String>,
    #[serde(default = "default_json")]
    pub format: String,
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test mcp_tools`
Expected: All PASS

- [ ] **Step 5: Commit**

```bash
git add src/mcp/ tests/mcp_tools.rs
git commit -m "feat: MCP log query tools (get_recent_logs, get_log_context, export_logs, clear_logs)"
```

---

## Task 13: MCP Filter + Trigger Management Tools

**Files:**
- Modify: `src/mcp/server.rs`
- Create: `src/mcp/tools_filters.rs` (helper functions)
- Create: `src/mcp/tools_triggers.rs` (helper functions)

- [ ] **Step 1: Implement filter management tools**

Add `get_filters`, `add_filter`, `edit_filter`, `remove_filter` as `#[tool]` methods.

- [ ] **Step 2: Implement trigger management tools**

Add `get_triggers`, `add_trigger`, `edit_trigger`, `remove_trigger` as `#[tool]` methods.

- [ ] **Step 3: Verify compilation and existing tests still pass**

Run: `cargo test`
Expected: All tests PASS

- [ ] **Step 4: Commit**

```bash
git add src/mcp/
git commit -m "feat: MCP filter and trigger management tools"
```

---

## Task 14: MCP Notifications

**Files:**
- Create: `src/mcp/notifications.rs`
- Modify: `src/engine/pipeline.rs` (add notification channel)

- [ ] **Step 1: Add notification channel to pipeline**

Modify `LogPipeline` to accept a `tokio::sync::broadcast::Sender<PipelineEvent>`. When `process()` produces events, send them on the channel.

- [ ] **Step 2: Implement notification dispatcher**

`src/mcp/notifications.rs`:
- Spawn a task that receives from the broadcast channel
- For each `PipelineEvent`, format the notification JSON per spec
- Send via `peer.send_notification()` (using the `Peer` handle from rmcp)

The notification JSON includes:
```json
{
    "trigger_id": 2,
    "trigger_description": "...",
    "filter": "...",
    "matched_entry": {},
    "context_before": [],
    "pre_trigger_flushed": 500,
    "post_window_size": 200,
    "buffer_size": 4523
}
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build`
Expected: Compiles

- [ ] **Step 4: Commit**

```bash
git add src/mcp/notifications.rs src/engine/pipeline.rs
git commit -m "feat: MCP trigger notifications"
```

---

## Task 15: CLI Configuration

**Files:**
- Create: `src/config.rs`

- [ ] **Step 1: Implement Config with clap**

```rust
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "gelf-mcp-server")]
#[command(about = "MCP server that collects GELF logs and exposes them to Claude")]
pub struct Config {
    /// Port for both UDP and TCP listeners
    #[arg(long, default_value = "12201", env = "GELF_PORT")]
    pub port: u16,

    /// Override port for UDP only
    #[arg(long, env = "GELF_UDP_PORT")]
    pub udp_port: Option<u16>,

    /// Override port for TCP only
    #[arg(long, env = "GELF_TCP_PORT")]
    pub tcp_port: Option<u16>,

    /// Max log entries in ring buffer
    #[arg(long, default_value = "10000", env = "GELF_BUFFER_SIZE")]
    pub buffer_size: usize,
}

impl Config {
    pub fn udp_port(&self) -> u16 {
        self.udp_port.unwrap_or(self.port)
    }
    pub fn tcp_port(&self) -> u16 {
        self.tcp_port.unwrap_or(self.port)
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build`
Expected: Compiles

- [ ] **Step 3: Commit**

```bash
git add src/config.rs
git commit -m "feat: CLI configuration with clap"
```

---

## Task 16: Main — Wire Everything Together

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Implement main function**

```rust
use clap::Parser;
use rmcp::ServiceExt;
use std::sync::Arc;

mod config;
mod engine;
mod filter;
mod gelf;
mod mcp;
mod store;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize stderr logging (stdout is reserved for MCP)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("gelf_mcp_server=info".parse()?)
        )
        .init();

    let config = config::Config::parse();

    let pipeline = Arc::new(engine::pipeline::LogPipeline::new(config.buffer_size));

    // Start GELF listeners
    let udp_addr = format!("0.0.0.0:{}", config.udp_port());
    let tcp_addr = format!("0.0.0.0:{}", config.tcp_port());

    let udp = gelf::udp::start_udp_listener(&udp_addr, pipeline.clone()).await?;
    let tcp = gelf::tcp::start_tcp_listener(&tcp_addr, pipeline.clone()).await?;

    eprintln!("GELF listeners: UDP={}, TCP={}", udp.local_addr, tcp.local_addr);

    // Start MCP server on stdio
    let server = mcp::server::GelfMcpServer::new(
        pipeline.clone(),
        udp.local_addr,
        tcp.local_addr,
    );

    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let mcp = server.serve(transport).await?;

    eprintln!("MCP server running on stdio");
    mcp.waiting().await?;

    Ok(())
}
```

- [ ] **Step 2: Verify full build**

Run: `cargo build`
Expected: Compiles

- [ ] **Step 3: Quick smoke test**

Run: `echo '{}' | cargo run -- --port 0 2>/dev/null || true`
Expected: Starts and exits (no panic)

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: wire everything together in main"
```

---

## Task 17: Integration Test — End to End

**Files:**
- Modify: `tests/pipeline.rs` (add full E2E tests)

- [ ] **Step 1: Write E2E test with GELF + pipeline + query**

```rust
#[tokio::test]
async fn test_end_to_end_udp_to_query() {
    let pipeline = Arc::new(LogPipeline::new(1000));

    let udp = gelf_mcp_server::gelf::udp::start_udp_listener(
        "127.0.0.1:0", pipeline.clone()
    ).await.unwrap();

    // Send various GELF messages
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    for i in 0..20 {
        let level = if i % 5 == 0 { 3 } else { 6 };
        let msg = serde_json::json!({
            "version": "1.1",
            "host": "e2e-test",
            "short_message": format!("message {i}"),
            "level": level,
            "facility": "test::e2e"
        });
        socket.send_to(
            msg.to_string().as_bytes(),
            format!("127.0.0.1:{}", udp.local_addr)
        ).unwrap();
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify all received
    assert_eq!(pipeline.store().len(), 20);

    // Verify trigger fired for errors
    let triggers = pipeline.list_triggers();
    assert!(triggers[0].match_count > 0); // error trigger fired

    // Query with filter
    use gelf_mcp_server::filter::parser::parse_filter;
    let filter = parse_filter("l>=ERROR").unwrap();
    let errors = pipeline.store().recent(100, Some(&filter));
    assert_eq!(errors.len(), 4); // messages 0, 5, 10, 15
}
```

- [ ] **Step 2: Run test**

Run: `cargo test --test pipeline test_end_to_end`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add tests/pipeline.rs
git commit -m "test: end-to-end integration test"
```

---

## Task 18: Claude Code Skill

**Files:**
- Create: `skill/gelf-logs.md`

- [ ] **Step 1: Write the skill file**

`skill/gelf-logs.md`:
```markdown
---
name: gelf-logs
description: Use when debugging applications that emit GELF logs, when you need to examine runtime behavior, error patterns, or investigate issues using live log data from the GELF MCP server.
---

# Using the GELF Log Collector

You have access to a GELF log collector MCP server that receives structured logs from applications in real-time.

## Available Tools

### Querying Logs
- **get_recent_logs** — Fetch recent logs, optionally filtered. Use `count` (default 100) and `filter` (DSL string).
- **get_log_context** — Get logs around a specific entry by `seq` number or `timestamp`.
- **export_logs** — Save logs to a file for comparison or sharing.
- **clear_logs** — Clear the log buffer (useful before running a test).

### Filtering
- **get_filters** — List active buffer filters.
- **add_filter** — Add a buffer filter. Only matching logs are stored. Use when you want to focus on specific components.
- **remove_filter** — Remove a filter. When no filters exist, all logs are buffered.

### Triggers
- **get_triggers** — List configured triggers.
- **add_trigger** — Add a trigger that notifies you when matching logs arrive. Set `pre_window` and `post_window` for context capture.
- **edit_trigger** — Modify a trigger's filter, windows, or description.
- **remove_trigger** — Remove a trigger.

### Status
- **get_status** — Server status: buffer size, filter/trigger counts, ports, message stats.

## Filter DSL

Filters use a comma-separated qualifier syntax (AND semantics):

| Pattern | Meaning |
|---------|---------|
| `text` | Case-insensitive substring match against all fields |
| `/regex/` | Regex match (case-sensitive, use `/regex/i` for insensitive) |
| `selector=pattern` | Match against a specific field |
| `l>=LEVEL` | Level comparison (ERROR, WARN, INFO, DEBUG, TRACE) |
| `"quoted"` | Literal text (use for commas or equals in patterns) |

**Selectors:** `m` (message), `fm` (full_message), `mfm` (message or full_message), `h` (host), `fa` (facility), `fi` (file), `ln` (line), `l` (level), or any custom GELF field name.

**Examples:**
- `l>=ERROR` — all errors
- `fa=mqtt,l>=WARN` — warnings+ from MQTT module
- `connection refused,h=myapp` — connection errors from myapp
- `/panic|unwrap failed/` — regex for panics

## Workflow Tips

### Debugging a specific component
1. `add_filter` with `fa=<module>` to focus on that module
2. Ask the user to reproduce the issue
3. `get_recent_logs` to examine what happened
4. `remove_filter` when done to go back to capturing everything

### Before/after comparison
1. `clear_logs` to start fresh
2. `export_logs` with path for "before" state
3. Make changes
4. `export_logs` with a different path for "after" state
5. Compare the two files

### When you receive a trigger notification
The notification includes `context_before` entries and the matched log. Use `get_log_context` with the matched entry's `seq` to get more surrounding context if needed.

## Important Notes
- Triggers always evaluate every incoming log, even when filters are active
- The pre-trigger buffer captures ALL logs regardless of filters — when a trigger fires, you get unfiltered context before the event
- Post-trigger windows also bypass filters, capturing the aftermath
- Logs during a post-trigger window skip trigger evaluation (prevents cascading)
```

- [ ] **Step 2: Commit**

```bash
git add skill/gelf-logs.md
git commit -m "feat: Claude Code skill for using the GELF MCP server"
```

---

## Task 19: Final Polish

- [ ] **Step 1: Run full test suite**

Run: `cargo test`
Expected: All tests PASS

- [ ] **Step 2: Run clippy**

Run: `cargo clippy -- -D warnings`
Expected: No warnings

- [ ] **Step 3: Fix any clippy issues found**

- [ ] **Step 4: Final commit**

```bash
git add -A
git commit -m "chore: clippy fixes and polish"
```

- [ ] **Step 5: Verify the binary runs**

Run: `cargo build --release && ls -la target/release/gelf-mcp-server`
Expected: Binary exists
