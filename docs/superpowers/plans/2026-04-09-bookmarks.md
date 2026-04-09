# Bookmarks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add named bookmarks to logmon — global timestamp anchors usable inside the filter DSL via `b>=name` / `b<=name`, so users can scope log/trace queries to a range without destructively clearing logs.

**Architecture:** A new in-memory `BookmarkStore` lives alongside `LogPipeline` and `SpanStore` on the daemon. Bookmarks are qualified by session name (`session/name`) so they cannot collide. The filter parser gains a `BookmarkFilter` qualifier; a new resolver step rewrites it to a `TimestampFilter` against the bookmark store before query execution. Bookmarks auto-evict when both log and span stores have rolled past their timestamp. Three new MCP tools (`add_bookmark`, `list_bookmarks`, `remove_bookmark`) front the store.

**Tech Stack:** Rust, chrono, serde, rmcp, the existing parser/matcher in `src/filter/`.

**Spec:** `docs/superpowers/specs/2026-04-09-bookmarks-design.md`

---

## File Structure

**New files:**

- `src/store/bookmarks.rs` — `BookmarkStore`, `Bookmark`, `BookmarkError`, the auto-eviction sweep, and the `qualify()` helper used by both the resolver and `bookmarks.remove`.
- `src/filter/bookmark_resolver.rs` — `resolve_bookmarks()` function and `BookmarkResolutionError`. Walks a `ParsedFilter` and rewrites `BookmarkFilter` qualifiers to `TimestampFilter`.

**Modified files:**

- `src/store/mod.rs` — register the new module.
- `src/filter/mod.rs` — register the new module.
- `src/filter/parser.rs` — add `BookmarkFilter` and `TimestampFilter` variants to `Qualifier`; add `BookmarkOp`; add `b>=` / `b<=` parsing in `parse_token`; add `EmptyBookmarkName` and `InvalidBookmarkName` error variants; update `is_log_qualifier` / `is_span_qualifier` to ignore the new variants.
- `src/filter/matcher.rs` — add `TimestampFilter` arms in `matches_qualifier` and `matches_span_qualifier` (compares against `entry.timestamp` / `span.start_time`); add `BookmarkFilter` arms returning `false` (should never reach the matcher post-resolution, but the match must be exhaustive).
- `src/store/memory.rs` — add `oldest_timestamp() -> Option<DateTime<Utc>>`.
- `src/store/traits.rs` — add `oldest_timestamp` to the `LogStore` trait.
- `src/span/store.rs` — add `oldest_timestamp() -> Option<DateTime<Utc>>`.
- `src/engine/pipeline.rs` — add a `oldest_log_timestamp()` accessor and update `recent_logs_str` to take an already-resolved `ParsedFilter` (or add a new function — see Task 11).
- `src/daemon/server.rs` — construct `BookmarkStore`, pass to `RpcHandler::new`.
- `src/daemon/rpc_handler.rs` — hold `Arc<BookmarkStore>`; add `bookmarks.add` / `bookmarks.list` / `bookmarks.remove` handlers; resolve bookmarks in every log/trace query handler; add the registration guard for `filters.add` / `triggers.add`.
- `src/mcp/server.rs` — add three new MCP tools.
- `README.md` — feature list + example.
- `skill/logmon.md` — bookmarks section.

---

## Task 1: BookmarkStore data model and basic CRUD

**Files:**

- Create: `src/store/bookmarks.rs`
- Modify: `src/store/mod.rs`
- Test: inline `#[cfg(test)] mod tests` in `src/store/bookmarks.rs`

- [ ] **Step 1: Register the new module**

Edit `src/store/mod.rs`:

```rust
pub mod traits;
pub mod memory;
pub mod bookmarks;
```

- [ ] **Step 2: Write failing tests for add/list/remove/replace**

Create `src/store/bookmarks.rs`:

```rust
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::RwLock;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Bookmark {
    pub qualified_name: String,
    pub name: String,
    pub session: String,
    pub timestamp: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum BookmarkError {
    #[error("invalid bookmark name: {0}")]
    InvalidName(String),
    #[error("bookmark already exists: {0}")]
    AlreadyExists(String),
    #[error("bookmark not found: {0}")]
    NotFound(String),
}

pub struct BookmarkStore {
    bookmarks: RwLock<HashMap<String, Bookmark>>,
}

impl BookmarkStore {
    pub fn new() -> Self {
        Self { bookmarks: RwLock::new(HashMap::new()) }
    }
}

impl Default for BookmarkStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_then_list_returns_bookmark() {
        let store = BookmarkStore::new();
        let b = store.add("A", "before", false).unwrap();
        assert_eq!(b.qualified_name, "A/before");
        assert_eq!(b.session, "A");
        assert_eq!(b.name, "before");
        let all = store.list();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].qualified_name, "A/before");
    }

    #[test]
    fn add_duplicate_without_replace_errors() {
        let store = BookmarkStore::new();
        store.add("A", "x", false).unwrap();
        let err = store.add("A", "x", false).unwrap_err();
        assert!(matches!(err, BookmarkError::AlreadyExists(ref n) if n == "A/x"));
    }

    #[test]
    fn add_duplicate_with_replace_overwrites_timestamp() {
        let store = BookmarkStore::new();
        let first = store.add("A", "x", false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = store.add("A", "x", true).unwrap();
        assert!(second.timestamp > first.timestamp);
    }

    #[test]
    fn invalid_name_rejected() {
        let store = BookmarkStore::new();
        assert!(matches!(store.add("A", "", false), Err(BookmarkError::InvalidName(_))));
        assert!(matches!(store.add("A", "has/slash", false), Err(BookmarkError::InvalidName(_))));
        assert!(matches!(store.add("A", "has space", false), Err(BookmarkError::InvalidName(_))));
    }

    #[test]
    fn remove_existing_ok() {
        let store = BookmarkStore::new();
        store.add("A", "x", false).unwrap();
        store.remove("A/x").unwrap();
        assert!(store.list().is_empty());
    }

    #[test]
    fn remove_missing_errors() {
        let store = BookmarkStore::new();
        assert!(matches!(store.remove("A/x"), Err(BookmarkError::NotFound(_))));
    }

    #[test]
    fn qualify_helper() {
        assert_eq!(qualify("foo", "A"), "A/foo");
        assert_eq!(qualify("B/foo", "A"), "B/foo");
    }
}
```

- [ ] **Step 3: Run tests, verify they fail to compile**

Run: `cargo test -p logmon-mcp --lib store::bookmarks`
Expected: compilation errors for missing methods (`add`, `list`, `remove`, `qualify`).

- [ ] **Step 4: Implement `add`, `list`, `remove`, `qualify`, name validation**

Add to `src/store/bookmarks.rs` (above the `#[cfg(test)]` block):

```rust
/// Validate a bare bookmark name (the user-supplied form).
/// Allowed: ASCII alphanumerics, '-', '_'. Non-empty. Max 64 chars.
pub fn is_valid_bookmark_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Resolve a name (bare or already-qualified) into a qualified name.
/// Bare names get prefixed with `{current_session}/`.
/// Qualified names (containing `/`) are returned unchanged.
pub fn qualify(name: &str, current_session: &str) -> String {
    if name.contains('/') {
        name.to_string()
    } else {
        format!("{current_session}/{name}")
    }
}

impl BookmarkStore {
    pub fn add(&self, session: &str, name: &str, replace: bool) -> Result<Bookmark, BookmarkError> {
        if !is_valid_bookmark_name(name) {
            return Err(BookmarkError::InvalidName(name.to_string()));
        }
        let qualified_name = format!("{session}/{name}");
        let now = Utc::now();
        let bookmark = Bookmark {
            qualified_name: qualified_name.clone(),
            name: name.to_string(),
            session: session.to_string(),
            timestamp: now,
            created_at: now,
        };
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        if map.contains_key(&qualified_name) && !replace {
            return Err(BookmarkError::AlreadyExists(qualified_name));
        }
        map.insert(qualified_name, bookmark.clone());
        Ok(bookmark)
    }

    pub fn list(&self) -> Vec<Bookmark> {
        let map = self.bookmarks.read().expect("bookmarks lock poisoned");
        let mut v: Vec<Bookmark> = map.values().cloned().collect();
        v.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        v
    }

    pub fn remove(&self, qualified_name: &str) -> Result<(), BookmarkError> {
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        map.remove(qualified_name)
            .map(|_| ())
            .ok_or_else(|| BookmarkError::NotFound(qualified_name.to_string()))
    }

    /// Look up a bookmark by qualified name. Returns the bookmark if it exists.
    pub fn get(&self, qualified_name: &str) -> Option<Bookmark> {
        self.bookmarks
            .read()
            .expect("bookmarks lock poisoned")
            .get(qualified_name)
            .cloned()
    }
}
```

- [ ] **Step 5: Run tests, verify they pass**

Run: `cargo test -p logmon-mcp --lib store::bookmarks`
Expected: all 7 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/store/mod.rs src/store/bookmarks.rs
git commit -m "feat(store): add BookmarkStore with add/list/remove/qualify"
```

---

## Task 2: BookmarkStore auto-eviction sweep

**Files:**

- Modify: `src/store/bookmarks.rs`
- Test: inline tests in same file

- [ ] **Step 1: Write failing tests for `should_evict` and `sweep`**

Add to the `tests` module in `src/store/bookmarks.rs`:

```rust
    #[test]
    fn should_evict_when_both_stores_past_timestamp() {
        let bookmark_ts = Utc::now() - chrono::Duration::seconds(60);
        let oldest_log = Some(Utc::now() - chrono::Duration::seconds(10));
        let oldest_span = Some(Utc::now() - chrono::Duration::seconds(5));
        assert!(should_evict(bookmark_ts, oldest_log, oldest_span));
    }

    #[test]
    fn should_not_evict_when_log_store_still_covers() {
        let bookmark_ts = Utc::now() - chrono::Duration::seconds(60);
        let oldest_log = Some(Utc::now() - chrono::Duration::seconds(120));
        let oldest_span = Some(Utc::now() - chrono::Duration::seconds(5));
        assert!(!should_evict(bookmark_ts, oldest_log, oldest_span));
    }

    #[test]
    fn should_not_evict_when_span_store_still_covers() {
        let bookmark_ts = Utc::now() - chrono::Duration::seconds(60);
        let oldest_log = Some(Utc::now() - chrono::Duration::seconds(10));
        let oldest_span = Some(Utc::now() - chrono::Duration::seconds(120));
        assert!(!should_evict(bookmark_ts, oldest_log, oldest_span));
    }

    #[test]
    fn empty_store_counts_as_past() {
        // Both empty: bookmark should be evicted (no data behind it).
        let bookmark_ts = Utc::now() - chrono::Duration::seconds(60);
        assert!(should_evict(bookmark_ts, None, None));
    }

    #[test]
    fn sweep_removes_evictable_bookmarks() {
        let store = BookmarkStore::new();
        store.add("A", "old", false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.add("A", "new", false).unwrap();
        // Pretend both stores have evicted everything older than just-now,
        // so both bookmarks count as past — both should be removed.
        let cutoff = Utc::now() + chrono::Duration::seconds(1);
        store.sweep(Some(cutoff), Some(cutoff));
        assert!(store.list().is_empty());
    }

    #[test]
    fn sweep_keeps_bookmarks_with_data_behind_them() {
        let store = BookmarkStore::new();
        let b = store.add("A", "x", false).unwrap();
        // Oldest log is older than the bookmark — data still covers it.
        let oldest = b.timestamp - chrono::Duration::seconds(10);
        store.sweep(Some(oldest), Some(oldest));
        assert_eq!(store.list().len(), 1);
    }
```

- [ ] **Step 2: Run tests, verify they fail to compile**

Run: `cargo test -p logmon-mcp --lib store::bookmarks`
Expected: compilation errors for missing `should_evict` and `sweep`.

- [ ] **Step 3: Implement `should_evict` and `sweep`**

Add to `src/store/bookmarks.rs` (free function alongside `qualify`, plus method on `BookmarkStore`):

```rust
/// Predicate: should this bookmark be auto-evicted?
/// True when *both* the log store and the span store have evicted past the
/// bookmark's timestamp (i.e. the oldest entry in each store is newer than
/// the bookmark, or the store is empty).
pub fn should_evict(
    bookmark_ts: DateTime<Utc>,
    oldest_log_ts: Option<DateTime<Utc>>,
    oldest_span_ts: Option<DateTime<Utc>>,
) -> bool {
    let log_gone = oldest_log_ts.map_or(true, |t| t > bookmark_ts);
    let span_gone = oldest_span_ts.map_or(true, |t| t > bookmark_ts);
    log_gone && span_gone
}
```

Add to the `impl BookmarkStore` block:

```rust
    /// Remove every bookmark whose data has been evicted from both stores.
    pub fn sweep(
        &self,
        oldest_log_ts: Option<DateTime<Utc>>,
        oldest_span_ts: Option<DateTime<Utc>>,
    ) {
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        map.retain(|_, b| !should_evict(b.timestamp, oldest_log_ts, oldest_span_ts));
    }
```

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test -p logmon-mcp --lib store::bookmarks`
Expected: all tests PASS (13 total now).

- [ ] **Step 5: Commit**

```bash
git add src/store/bookmarks.rs
git commit -m "feat(store): add bookmark auto-eviction sweep"
```

---

## Task 3: `oldest_timestamp` accessors on log and span stores

**Files:**

- Modify: `src/store/traits.rs`
- Modify: `src/store/memory.rs`
- Modify: `src/span/store.rs`
- Modify: `src/engine/pipeline.rs`

- [ ] **Step 1: Write a failing test for `InMemoryStore::oldest_timestamp`**

Add to the existing `#[cfg(test)] mod tests` in `src/store/memory.rs` (if no test module exists, create one at the bottom of the file):

```rust
#[cfg(test)]
mod oldest_ts_tests {
    use super::*;
    use crate::gelf::message::{LogEntry, Level};
    use chrono::Utc;

    fn entry(seq: u64) -> LogEntry {
        LogEntry {
            seq,
            timestamp: Utc::now(),
            level: Level::Info,
            host: "h".to_string(),
            message: "m".to_string(),
            full_message: None,
            facility: None,
            file: None,
            line: None,
            additional_fields: Default::default(),
            source: crate::gelf::message::LogSource::Gelf,
            trace_id: None,
            span_id: None,
        }
    }

    #[test]
    fn oldest_timestamp_empty_store_returns_none() {
        let store = InMemoryStore::new(10);
        assert!(store.oldest_timestamp().is_none());
    }

    #[test]
    fn oldest_timestamp_returns_front_entry_timestamp() {
        let store = InMemoryStore::new(10);
        let mut e1 = entry(1);
        e1.timestamp = Utc::now() - chrono::Duration::seconds(60);
        let mut e2 = entry(2);
        e2.timestamp = Utc::now();
        store.append(e1.clone());
        store.append(e2);
        assert_eq!(store.oldest_timestamp(), Some(e1.timestamp));
    }
}
```

Note: if `LogEntry` has fields not listed above, copy them from the existing struct definition in `src/gelf/message.rs` and supply default-ish values. Run a quick `grep -n "pub struct LogEntry" src/gelf/message.rs` to confirm field list.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p logmon-mcp --lib store::memory::oldest_ts_tests`
Expected: compilation error — `oldest_timestamp` method does not exist.

- [ ] **Step 3: Add `oldest_timestamp` to the `LogStore` trait**

Edit `src/store/traits.rs`, add to the `LogStore` trait (after `stats`):

```rust
    fn oldest_timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>>;
```

- [ ] **Step 4: Implement `oldest_timestamp` on `InMemoryStore`**

Edit `src/store/memory.rs`. Add this method inside `impl LogStore for InMemoryStore`:

```rust
    fn oldest_timestamp(&self) -> Option<DateTime<Utc>> {
        self.entries.read().unwrap().front().map(|e| e.timestamp)
    }
```

- [ ] **Step 5: Run the log-store test, verify it passes**

Run: `cargo test -p logmon-mcp --lib store::memory::oldest_ts_tests`
Expected: both tests PASS.

- [ ] **Step 6: Add `oldest_timestamp` to `SpanStore` and write a test**

Edit `src/span/store.rs`. Add inside `impl SpanStore`:

```rust
    pub fn oldest_timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.inner.read().unwrap().buffer.front().map(|s| s.start_time)
    }
```

Add a test at the bottom of `src/span/store.rs` (in a new or existing `#[cfg(test)] mod tests` block):

```rust
#[cfg(test)]
mod oldest_ts_tests {
    use super::*;
    use crate::engine::seq_counter::SeqCounter;
    use std::sync::Arc;

    #[test]
    fn empty_span_store_returns_none() {
        let store = SpanStore::new(10, Arc::new(SeqCounter::new()));
        assert!(store.oldest_timestamp().is_none());
    }
}
```

Run: `cargo test -p logmon-mcp --lib span::store::oldest_ts_tests`
Expected: PASS.

- [ ] **Step 7: Add an accessor on `LogPipeline`**

Edit `src/engine/pipeline.rs`. Add inside `impl LogPipeline` (next to the existing `recent_logs` method):

```rust
    pub fn oldest_log_timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.store.oldest_timestamp()
    }
```

- [ ] **Step 8: Verify the workspace builds**

Run: `cargo build -p logmon-mcp`
Expected: clean build (no warnings related to this change).

- [ ] **Step 9: Commit**

```bash
git add src/store/traits.rs src/store/memory.rs src/span/store.rs src/engine/pipeline.rs
git commit -m "feat(store): add oldest_timestamp accessors on log/span stores"
```

---

## Task 4: Parser — `BookmarkFilter` and `TimestampFilter` qualifier variants

**Files:**

- Modify: `src/filter/parser.rs`
- Test: inline tests in `src/filter/parser.rs`

- [ ] **Step 1: Write failing tests for `b>=` / `b<=` parsing**

Add to the existing test module in `src/filter/parser.rs` (if no test module exists, create one at the bottom):

```rust
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
        // bookmark + log selector should NOT trip the mix check
        let parsed = parse_filter("b>=before, l>=warn").unwrap();
        if let ParsedFilter::Qualifiers(qs) = parsed {
            assert_eq!(qs.len(), 2);
        } else {
            panic!("expected qualifiers");
        }
    }

    #[test]
    fn bookmark_combines_with_span_qualifier_without_mix_error() {
        // bookmark + span/duration should NOT trip the mix check
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
}
```

- [ ] **Step 2: Run tests, verify they fail to compile**

Run: `cargo test -p logmon-mcp --lib filter::parser::bookmark_tests`
Expected: compilation errors for missing `BookmarkFilter`, `BookmarkOp`, `EmptyBookmarkName`, `InvalidBookmarkName`.

- [ ] **Step 3: Add the new qualifier variants and error variants**

Edit `src/filter/parser.rs`. Inside the `Qualifier` enum, add (and `derive(Debug)` is already on it):

```rust
    BookmarkFilter { op: BookmarkOp, name: String },
    /// Internal-only: produced by `resolve_bookmarks` from `BookmarkFilter`.
    /// Never emitted by the parser, never serialized.
    TimestampFilter { op: BookmarkOp, ts: chrono::DateTime<chrono::Utc> },
```

Add a new enum next to `DurationOp`:

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum BookmarkOp {
    Gte,
    Lte,
}
```

Add to `FilterParseError`:

```rust
    #[error("empty bookmark name")]
    EmptyBookmarkName,
    #[error("invalid bookmark name: {0}")]
    InvalidBookmarkName(String),
```

- [ ] **Step 4: Add `b>=` / `b<=` parsing in `parse_token`**

Edit `src/filter/parser.rs`. Inside `parse_token`, add these branches *immediately after* the existing duration branches (so they take precedence over the generic `selector=value` rule):

```rust
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
```

Add this helper as a free function near the top of the file (alongside `is_valid_session_name` if you decide to colocate, otherwise just inside `parser.rs`):

```rust
/// Validate a bookmark name as it appears inside a DSL filter token.
/// Allowed: ASCII alphanumerics, '-', '_', '/'. The '/' enables qualified
/// names like "session-A/before".
fn is_valid_bookmark_token(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'/')
}
```

- [ ] **Step 5: Update `is_log_qualifier` and `is_span_qualifier`**

Both helpers in `src/filter/parser.rs` already return `false` for unmatched variants, so the `BookmarkFilter` and `TimestampFilter` cases automatically fall through to `false` — meaning they're classified as neither log nor span and won't trip the "cannot mix" check. **Confirm this by reading both functions** and verifying neither has a wildcard `_` arm that maps to `true`. (They should have explicit arms returning `false` or be unchanged.)

If either function uses an exhaustive match without a wildcard, add explicit `false` arms:

```rust
        Qualifier::BookmarkFilter { .. } => false,
        Qualifier::TimestampFilter { .. } => false,
```

- [ ] **Step 6: Run parser tests, verify they pass**

Run: `cargo test -p logmon-mcp --lib filter::parser`
Expected: all bookmark tests PASS, no existing parser tests broken.

- [ ] **Step 7: Build the workspace to surface non-exhaustive match errors**

Run: `cargo build -p logmon-mcp`
Expected: errors from `src/filter/matcher.rs` about non-exhaustive matches in `matches_qualifier` and `matches_span_qualifier` (because we added new variants). These will be fixed in Task 5.

- [ ] **Step 8: Commit**

```bash
git add src/filter/parser.rs
git commit -m "feat(filter): add BookmarkFilter and TimestampFilter qualifier variants"
```

---

## Task 5: Matcher — `TimestampFilter` arms for logs and spans

**Files:**

- Modify: `src/filter/matcher.rs`
- Test: inline tests in `src/filter/matcher.rs`

- [ ] **Step 1: Write failing tests**

Add at the bottom of `src/filter/matcher.rs`:

```rust
#[cfg(test)]
mod timestamp_tests {
    use super::*;
    use crate::filter::parser::*;
    use crate::gelf::message::{LogEntry, Level, LogSource};
    use chrono::Utc;

    fn log_entry_at(ts: chrono::DateTime<chrono::Utc>) -> LogEntry {
        LogEntry {
            seq: 1,
            timestamp: ts,
            level: Level::Info,
            host: "h".into(),
            message: "m".into(),
            full_message: None,
            facility: None,
            file: None,
            line: None,
            additional_fields: Default::default(),
            source: LogSource::Gelf,
            trace_id: None,
            span_id: None,
        }
    }

    #[test]
    fn timestamp_gte_matches_entry_at_or_after() {
        let cutoff = Utc::now() - chrono::Duration::seconds(10);
        let q = Qualifier::TimestampFilter { op: BookmarkOp::Gte, ts: cutoff };
        assert!(matches_qualifier(&q, &log_entry_at(Utc::now())));
        assert!(!matches_qualifier(&q, &log_entry_at(cutoff - chrono::Duration::seconds(5))));
    }

    #[test]
    fn timestamp_lte_matches_entry_at_or_before() {
        let cutoff = Utc::now();
        let q = Qualifier::TimestampFilter { op: BookmarkOp::Lte, ts: cutoff };
        assert!(matches_qualifier(&q, &log_entry_at(cutoff - chrono::Duration::seconds(5))));
        assert!(!matches_qualifier(&q, &log_entry_at(cutoff + chrono::Duration::seconds(5))));
    }

    #[test]
    fn bookmark_filter_returns_false_in_matcher() {
        // BookmarkFilter should be resolved away before matching; if it
        // somehow reaches the matcher, it must return false (safe default).
        let q = Qualifier::BookmarkFilter {
            op: BookmarkOp::Gte,
            name: "x".into(),
        };
        assert!(!matches_qualifier(&q, &log_entry_at(Utc::now())));
    }
}
```

Note: the test calls `matches_qualifier` which is currently `pub(super)` or private. Make it `pub(crate)` so the test in this file can reach it (it's already in the same file, so visibility is fine — but if you split tests later, remember this).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p logmon-mcp --lib filter::matcher`
Expected: still failing to *compile* because of the non-exhaustive match from Task 4. That's the cue to fix the matcher.

- [ ] **Step 3: Add the new arms in `matches_qualifier`**

Edit `src/filter/matcher.rs`. Inside `matches_qualifier`, add:

```rust
        Qualifier::BookmarkFilter { .. } => false,
        Qualifier::TimestampFilter { op, ts } => match op {
            BookmarkOp::Gte => entry.timestamp >= *ts,
            BookmarkOp::Lte => entry.timestamp <= *ts,
        },
```

You'll need to import `BookmarkOp` at the top of the file (it's re-exported via `crate::filter::parser::*` already if that's how the file imports things — check the top of `matcher.rs`).

- [ ] **Step 4: Add the new arms in `matches_span_qualifier`**

In the same file, inside `matches_span_qualifier`, add:

```rust
        Qualifier::BookmarkFilter { .. } => false,
        Qualifier::TimestampFilter { op, ts } => match op {
            BookmarkOp::Gte => span.start_time >= *ts,
            BookmarkOp::Lte => span.start_time <= *ts,
        },
```

- [ ] **Step 5: Run matcher tests, verify they pass**

Run: `cargo test -p logmon-mcp --lib filter::matcher`
Expected: all PASS.

- [ ] **Step 6: Verify the whole workspace builds**

Run: `cargo build -p logmon-mcp`
Expected: clean build.

- [ ] **Step 7: Commit**

```bash
git add src/filter/matcher.rs
git commit -m "feat(filter): match TimestampFilter against entry/span timestamps"
```

---

## Task 6: Bookmark resolver

**Files:**

- Create: `src/filter/bookmark_resolver.rs`
- Modify: `src/filter/mod.rs`
- Test: inline tests in `src/filter/bookmark_resolver.rs`

- [ ] **Step 1: Register the new module**

Edit `src/filter/mod.rs`, add:

```rust
pub mod bookmark_resolver;
```

- [ ] **Step 2: Write failing tests**

Create `src/filter/bookmark_resolver.rs`:

```rust
use crate::filter::parser::{ParsedFilter, Qualifier};
use crate::store::bookmarks::{qualify, BookmarkStore};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BookmarkResolutionError {
    #[error("bookmark not found: {0}")]
    NotFound(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::parser::{parse_filter, BookmarkOp};

    #[test]
    fn bare_name_resolves_against_current_session() {
        let store = BookmarkStore::new();
        store.add("A", "before", false).unwrap();
        let filter = parse_filter("b>=before").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved {
            assert_eq!(qs.len(), 1);
            assert!(matches!(qs[0], Qualifier::TimestampFilter { op: BookmarkOp::Gte, .. }));
        } else {
            panic!("expected qualifiers");
        }
    }

    #[test]
    fn qualified_name_resolves_directly() {
        let store = BookmarkStore::new();
        store.add("B", "x", false).unwrap();
        let filter = parse_filter("b<=B/x").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved {
            assert!(matches!(qs[0], Qualifier::TimestampFilter { op: BookmarkOp::Lte, .. }));
        } else {
            panic!("expected qualifiers");
        }
    }

    #[test]
    fn missing_bookmark_returns_not_found() {
        let store = BookmarkStore::new();
        let filter = parse_filter("b>=ghost").unwrap();
        let err = resolve_bookmarks(filter, &store, "A").unwrap_err();
        assert!(matches!(err, BookmarkResolutionError::NotFound(ref n) if n == "A/ghost"));
    }

    #[test]
    fn non_bookmark_qualifiers_pass_through() {
        let store = BookmarkStore::new();
        let filter = parse_filter("l>=warn").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved {
            assert!(matches!(qs[0], Qualifier::LevelFilter { .. }));
        } else {
            panic!("expected qualifiers");
        }
    }

    #[test]
    fn multiple_bookmarks_all_resolved() {
        let store = BookmarkStore::new();
        store.add("A", "start", false).unwrap();
        store.add("A", "end", false).unwrap();
        let filter = parse_filter("b>=start, b<=end, l>=info").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved {
            assert_eq!(qs.len(), 3);
            assert!(matches!(qs[0], Qualifier::TimestampFilter { .. }));
            assert!(matches!(qs[1], Qualifier::TimestampFilter { .. }));
            assert!(matches!(qs[2], Qualifier::LevelFilter { .. }));
        } else {
            panic!("expected qualifiers");
        }
    }

    #[test]
    fn all_and_none_filters_pass_through_unchanged() {
        let store = BookmarkStore::new();
        let resolved = resolve_bookmarks(ParsedFilter::All, &store, "A").unwrap();
        assert!(matches!(resolved, ParsedFilter::All));
        let resolved = resolve_bookmarks(ParsedFilter::None, &store, "A").unwrap();
        assert!(matches!(resolved, ParsedFilter::None));
    }
}
```

- [ ] **Step 3: Run tests, verify they fail**

Run: `cargo test -p logmon-mcp --lib filter::bookmark_resolver`
Expected: compile error — `resolve_bookmarks` not defined.

- [ ] **Step 4: Implement `resolve_bookmarks`**

Add to `src/filter/bookmark_resolver.rs` (above the test module):

```rust
/// Walk a parsed filter and rewrite every `BookmarkFilter` qualifier to a
/// `TimestampFilter` by looking up the bookmark in the store. Bare names are
/// qualified against `current_session`. Returns `NotFound` on the first
/// missing bookmark.
pub fn resolve_bookmarks(
    filter: ParsedFilter,
    store: &BookmarkStore,
    current_session: &str,
) -> Result<ParsedFilter, BookmarkResolutionError> {
    match filter {
        ParsedFilter::All | ParsedFilter::None => Ok(filter),
        ParsedFilter::Qualifiers(qs) => {
            let mut out = Vec::with_capacity(qs.len());
            for q in qs {
                match q {
                    Qualifier::BookmarkFilter { op, name } => {
                        let qualified = qualify(&name, current_session);
                        let bookmark = store
                            .get(&qualified)
                            .ok_or(BookmarkResolutionError::NotFound(qualified))?;
                        out.push(Qualifier::TimestampFilter {
                            op,
                            ts: bookmark.timestamp,
                        });
                    }
                    other => out.push(other),
                }
            }
            Ok(ParsedFilter::Qualifiers(out))
        }
    }
}
```

- [ ] **Step 5: Run tests, verify they pass**

Run: `cargo test -p logmon-mcp --lib filter::bookmark_resolver`
Expected: all 6 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/filter/mod.rs src/filter/bookmark_resolver.rs
git commit -m "feat(filter): add bookmark resolver"
```

---

## Task 7: Wire `BookmarkStore` into the daemon

**Files:**

- Modify: `src/daemon/server.rs`
- Modify: `src/daemon/rpc_handler.rs`

- [ ] **Step 1: Construct `BookmarkStore` in `daemon/server.rs` and pass to `RpcHandler::new`**

Edit `src/daemon/server.rs`. Find the block that creates `RpcHandler` (around line 119) and add the bookmark store construction just above it:

```rust
    // 13a. Create BookmarkStore (in-memory only, not persisted)
    let bookmark_store = Arc::new(crate::store::bookmarks::BookmarkStore::new());

    // 13. Create RpcHandler
    let handler = Arc::new(RpcHandler::new(
        pipeline.clone(),
        span_store.clone(),
        sessions.clone(),
        bookmark_store.clone(),
        all_receivers_info,
    ));
```

- [ ] **Step 2: Add the field and constructor parameter on `RpcHandler`**

Edit `src/daemon/rpc_handler.rs`. Update the struct:

```rust
pub struct RpcHandler {
    pipeline: Arc<LogPipeline>,
    span_store: Arc<SpanStore>,
    sessions: Arc<SessionRegistry>,
    bookmarks: Arc<crate::store::bookmarks::BookmarkStore>,
    start_time: std::time::Instant,
    receivers_info: Vec<String>,
}
```

Update `RpcHandler::new`:

```rust
impl RpcHandler {
    pub fn new(
        pipeline: Arc<LogPipeline>,
        span_store: Arc<SpanStore>,
        sessions: Arc<SessionRegistry>,
        bookmarks: Arc<crate::store::bookmarks::BookmarkStore>,
        receivers_info: Vec<String>,
    ) -> Self {
        Self {
            pipeline,
            span_store,
            sessions,
            bookmarks,
            start_time: std::time::Instant::now(),
            receivers_info,
        }
    }
```

- [ ] **Step 3: Build the workspace**

Run: `cargo build -p logmon-mcp`
Expected: clean build (no usages yet, so the field shows as unused — that's fine for one step; we'll use it next).

- [ ] **Step 4: Commit**

```bash
git add src/daemon/server.rs src/daemon/rpc_handler.rs
git commit -m "feat(daemon): construct and wire BookmarkStore into RpcHandler"
```

---

## Task 8: RPC + MCP tool — `bookmarks.add` / `add_bookmark`

**Files:**

- Modify: `src/daemon/rpc_handler.rs`
- Modify: `src/mcp/server.rs`

- [ ] **Step 1: Add the RPC method dispatch entry**

Edit `src/daemon/rpc_handler.rs`. Inside `RpcHandler::handle`, add a new arm to the dispatch match:

```rust
            "bookmarks.add" => self.handle_bookmarks_add(session_id, &request.params),
```

- [ ] **Step 2: Add the handler method**

Add at the bottom of the `impl RpcHandler` block in `src/daemon/rpc_handler.rs`:

```rust
    // -----------------------------------------------------------------------
    // bookmarks.*
    // -----------------------------------------------------------------------

    fn handle_bookmarks_add(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: name".to_string())?;
        let replace = params
            .get("replace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Sweep before adding so the store stays tidy.
        self.sweep_bookmarks();

        let session = session_id.to_string();
        let bookmark = self
            .bookmarks
            .add(&session, name, replace)
            .map_err(|e| e.to_string())?;
        let replaced = replace; // we only know "user asked for replace"; the store-level distinction isn't needed by the caller
        Ok(json!({
            "qualified_name": bookmark.qualified_name,
            "timestamp": bookmark.timestamp,
            "replaced": replaced,
        }))
    }

    fn sweep_bookmarks(&self) {
        let oldest_log = self.pipeline.oldest_log_timestamp();
        let oldest_span = self.span_store.oldest_timestamp();
        self.bookmarks.sweep(oldest_log, oldest_span);
    }
```

- [ ] **Step 3: Add the MCP tool wrapper**

Edit `src/mcp/server.rs`. Add a parameter struct near the other parameter structs (after `DropSessionParams`):

```rust
#[derive(Deserialize, JsonSchema)]
struct AddBookmarkParams {
    /// Bookmark name (alphanumerics, '-', '_'; max 64 chars). Will be qualified
    /// with the calling session's name automatically.
    name: String,
    /// If true, overwrite an existing bookmark with the same qualified name.
    replace: Option<bool>,
}
```

Add the tool method inside the `#[rmcp::tool_router] impl GelfMcpServer` block (e.g. just before `// ---- Session Management Tools ----`):

```rust
    // ---- Bookmark Tools ----

    #[rmcp::tool(description = "Set a named bookmark at the current moment. Bookmarks are timestamps usable in filter DSL via b>=name / b<=name. Use them to scope queries to a range without destructively clearing logs.")]
    async fn add_bookmark(
        &self,
        Parameters(p): Parameters<AddBookmarkParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call(
                "bookmarks.add",
                serde_json::json!({
                    "name": p.name,
                    "replace": p.replace,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }
```

- [ ] **Step 4: Build the workspace**

Run: `cargo build -p logmon-mcp`
Expected: clean build.

- [ ] **Step 5: Commit**

```bash
git add src/daemon/rpc_handler.rs src/mcp/server.rs
git commit -m "feat(mcp): add add_bookmark tool"
```

---

## Task 9: RPC + MCP tools — `bookmarks.list` / `list_bookmarks` and `bookmarks.remove` / `remove_bookmark`

**Files:**

- Modify: `src/daemon/rpc_handler.rs`
- Modify: `src/mcp/server.rs`

- [ ] **Step 1: Add the RPC dispatch entries**

In `RpcHandler::handle` in `src/daemon/rpc_handler.rs`, add two more arms:

```rust
            "bookmarks.list" => self.handle_bookmarks_list(&request.params),
            "bookmarks.remove" => self.handle_bookmarks_remove(session_id, &request.params),
```

- [ ] **Step 2: Add the two handler methods**

Append to the `bookmarks.*` block in `src/daemon/rpc_handler.rs`:

```rust
    fn handle_bookmarks_list(&self, params: &Value) -> Result<Value, String> {
        self.sweep_bookmarks();
        let session_filter = params.get("session").and_then(|v| v.as_str());

        let now = chrono::Utc::now();
        let items: Vec<Value> = self
            .bookmarks
            .list()
            .into_iter()
            .filter(|b| session_filter.map_or(true, |s| b.session == s))
            .map(|b| {
                let age = (now - b.timestamp).num_seconds().max(0);
                json!({
                    "qualified_name": b.qualified_name,
                    "name": b.name,
                    "session": b.session,
                    "timestamp": b.timestamp,
                    "age_secs": age,
                })
            })
            .collect();

        Ok(json!({ "bookmarks": items, "count": items.len() }))
    }

    fn handle_bookmarks_remove(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: name".to_string())?;
        let qualified = crate::store::bookmarks::qualify(name, &session_id.to_string());
        self.bookmarks
            .remove(&qualified)
            .map_err(|e| e.to_string())?;
        Ok(json!({ "removed": qualified }))
    }
```

- [ ] **Step 3: Add the MCP tool wrappers**

Edit `src/mcp/server.rs`. Add parameter structs near `AddBookmarkParams`:

```rust
#[derive(Deserialize, JsonSchema)]
struct ListBookmarksParams {
    /// Optional: filter to bookmarks created by this session name.
    session: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RemoveBookmarkParams {
    /// Bare name (resolved against current session) or qualified "session/name".
    name: String,
}
```

Add the two tool methods next to `add_bookmark`:

```rust
    #[rmcp::tool(description = "List all live bookmarks across all sessions, newest first. Optionally filter by session name.")]
    async fn list_bookmarks(
        &self,
        Parameters(p): Parameters<ListBookmarksParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call(
                "bookmarks.list",
                serde_json::json!({ "session": p.session }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    #[rmcp::tool(description = "Remove a bookmark by name. Bare name resolves to the current session; use 'session/name' to remove a bookmark from another session.")]
    async fn remove_bookmark(
        &self,
        Parameters(p): Parameters<RemoveBookmarkParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self
            .bridge
            .call(
                "bookmarks.remove",
                serde_json::json!({ "name": p.name }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }
```

- [ ] **Step 4: Build the workspace**

Run: `cargo build -p logmon-mcp`
Expected: clean build.

- [ ] **Step 5: Commit**

```bash
git add src/daemon/rpc_handler.rs src/mcp/server.rs
git commit -m "feat(mcp): add list_bookmarks and remove_bookmark tools"
```

---

## Task 10: Wire bookmark resolution into log/trace query handlers

**Files:**

- Modify: `src/daemon/rpc_handler.rs`
- Modify: `src/engine/pipeline.rs`

The query handlers currently take a `filter: Option<&str>`, parse it inside the helper, and pass it to the store/span methods. We need to insert a resolution step between parse and execute, which means each handler needs to: parse the filter string here, resolve it against the bookmark store using the calling session's name, then call into the store with a `Option<&ParsedFilter>`.

- [ ] **Step 1: Add a helper on `RpcHandler` to parse-and-resolve a filter string**

Add inside `impl RpcHandler` in `src/daemon/rpc_handler.rs` (next to `sweep_bookmarks`):

```rust
    /// Parse a filter string and resolve any bookmark qualifiers against the
    /// bookmark store using `session_id` as the current session. Returns
    /// `Ok(None)` if the input is `None`.
    fn parse_and_resolve_filter(
        &self,
        filter_str: Option<&str>,
        session_id: &SessionId,
    ) -> Result<Option<crate::filter::parser::ParsedFilter>, String> {
        let Some(s) = filter_str else { return Ok(None) };
        let parsed = crate::filter::parser::parse_filter(s).map_err(|e| e.to_string())?;
        let resolved = crate::filter::bookmark_resolver::resolve_bookmarks(
            parsed,
            &self.bookmarks,
            &session_id.to_string(),
        )
        .map_err(|e| e.to_string())?;
        Ok(Some(resolved))
    }
```

- [ ] **Step 2: Update `handle_logs_recent`**

Rewrite the body to use the resolver. Replace the existing `handle_logs_recent` in `src/daemon/rpc_handler.rs`:

```rust
    fn handle_logs_recent(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let count = params.get("count").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let filter_str = params.get("filter").and_then(|v| v.as_str());

        // Optional trace_id filter (unchanged)
        if let Some(trace_id_hex) = params.get("trace_id").and_then(|v| v.as_str()) {
            let trace_id = u128::from_str_radix(trace_id_hex, 16)
                .map_err(|_| "invalid trace_id")?;
            let logs = self.pipeline.logs_by_trace_id(trace_id);
            return Ok(json!({ "logs": logs, "count": logs.len() }));
        }

        let resolved = self.parse_and_resolve_filter(filter_str, session_id)?;
        let entries = self.pipeline.recent_logs(count, resolved.as_ref());
        Ok(json!({ "logs": entries, "count": entries.len() }))
    }
```

Then update the dispatch arm for `"logs.recent"` in `RpcHandler::handle` to pass `session_id`:

```rust
            "logs.recent" => self.handle_logs_recent(session_id, &request.params),
```

- [ ] **Step 3: Update `handle_logs_export` similarly**

Replace the body:

```rust
    fn handle_logs_export(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let count = params
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(u64::MAX) as usize;
        let filter_str = params.get("filter").and_then(|v| v.as_str());
        let resolved = self.parse_and_resolve_filter(filter_str, session_id)?;
        let entries = self.pipeline.recent_logs(count, resolved.as_ref());
        Ok(json!({ "logs": entries, "count": entries.len(), "format": "json" }))
    }
```

Update the dispatch arm:

```rust
            "logs.export" => self.handle_logs_export(session_id, &request.params),
```

- [ ] **Step 4: Update `handle_traces_recent`**

Replace:

```rust
    fn handle_traces_recent(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let count = params.get("count").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let filter_str = params.get("filter").and_then(|v| v.as_str());
        let resolved = self.parse_and_resolve_filter(filter_str, session_id)?;

        let pipeline = &self.pipeline;
        let summaries = self.span_store.recent_traces(
            count,
            resolved.as_ref(),
            |trace_id| pipeline.count_by_trace_id(trace_id) as u32,
        );
        Ok(json!({ "traces": summaries, "count": summaries.len() }))
    }
```

Update dispatch arm:

```rust
            "traces.recent" => self.handle_traces_recent(session_id, &request.params),
```

- [ ] **Step 5: Update `handle_traces_get`**

The current implementation supports a `filter` param. Update to resolve bookmarks before applying the filter to spans. Replace:

```rust
    fn handle_traces_get(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let trace_id_hex = params
            .get("trace_id")
            .and_then(|v| v.as_str())
            .ok_or("missing required parameter: trace_id")?;
        let trace_id = u128::from_str_radix(trace_id_hex, 16)
            .map_err(|_| "invalid trace_id: must be 32-char hex")?;
        let include_logs = params
            .get("include_logs")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Resolve filter (used to filter spans within the trace below)
        let filter_str = params.get("filter").and_then(|v| v.as_str());
        let resolved = self.parse_and_resolve_filter(filter_str, session_id)?;

        let mut spans = self.span_store.get_trace(trace_id);
        if let Some(f) = resolved.as_ref() {
            spans.retain(|s| crate::filter::matcher::matches_span(f, s));
        }
        let logs = if include_logs {
            self.pipeline.logs_by_trace_id(trace_id)
        } else {
            vec![]
        };

        Ok(json!({
            "trace_id": trace_id_hex,
            "spans": spans,
            "logs": logs,
            "span_count": spans.len(),
            "log_count": logs.len(),
        }))
    }
```

Update dispatch:

```rust
            "traces.get" => self.handle_traces_get(session_id, &request.params),
```

- [ ] **Step 6: Update `handle_traces_slow`**

Replace the filter parsing block (lines starting with `let filter_str = ...`):

```rust
    fn handle_traces_slow(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let min_duration = params
            .get("min_duration_ms")
            .and_then(|v| v.as_f64())
            .unwrap_or(100.0);
        let count = params.get("count").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let filter_str = params.get("filter").and_then(|v| v.as_str());
        let resolved = self.parse_and_resolve_filter(filter_str, session_id)?;
        let group_by = params.get("group_by").and_then(|v| v.as_str());

        let slow = self
            .span_store
            .slow_spans(min_duration, count, resolved.as_ref());

        // ... existing match group_by { ... } block stays unchanged ...
```

(Keep the rest of the function as it was — only the filter resolution lines change.)

Update dispatch:

```rust
            "traces.slow" => self.handle_traces_slow(session_id, &request.params),
```

- [ ] **Step 7: Update `handle_traces_logs`**

Replace:

```rust
    fn handle_traces_logs(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let trace_id_hex = params
            .get("trace_id")
            .and_then(|v| v.as_str())
            .ok_or("missing required parameter: trace_id")?;
        let trace_id =
            u128::from_str_radix(trace_id_hex, 16).map_err(|_| "invalid trace_id")?;

        let filter_str = params.get("filter").and_then(|v| v.as_str());
        let resolved = self.parse_and_resolve_filter(filter_str, session_id)?;

        let mut logs = self.pipeline.logs_by_trace_id(trace_id);
        if let Some(f) = resolved.as_ref() {
            logs.retain(|e| crate::filter::matcher::matches_entry(f, e));
        }
        Ok(json!({ "logs": logs, "count": logs.len() }))
    }
```

Update dispatch:

```rust
            "traces.logs" => self.handle_traces_logs(session_id, &request.params),
```

- [ ] **Step 8: Build the workspace**

Run: `cargo build -p logmon-mcp`
Expected: clean build. If `recent_logs_str` is now unused, leave it for now (other callers may rely on it; remove only if `cargo build` flags it as unused with `-D warnings`).

- [ ] **Step 9: Run the existing test suite to confirm nothing regressed**

Run: `cargo test -p logmon-mcp --lib`
Expected: all PASS.

- [ ] **Step 10: Commit**

```bash
git add src/daemon/rpc_handler.rs
git commit -m "feat(daemon): resolve bookmark filters in log/trace query handlers"
```

---

## Task 11: Registration guard — block bookmark filters in `filters.add` / `triggers.add`

**Files:**

- Modify: `src/filter/parser.rs` (add a helper)
- Modify: `src/daemon/rpc_handler.rs`
- Test: inline tests in `src/filter/parser.rs`

- [ ] **Step 1: Write failing tests for `contains_bookmark_qualifier`**

Add to the `bookmark_tests` module in `src/filter/parser.rs`:

```rust
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
```

- [ ] **Step 2: Run, verify they fail to compile**

Run: `cargo test -p logmon-mcp --lib filter::parser::bookmark_tests`
Expected: missing `contains_bookmark_qualifier`.

- [ ] **Step 3: Implement the helper**

Add as a `pub` free function near the bottom of `src/filter/parser.rs`:

```rust
/// Returns true if any qualifier in the filter is a `BookmarkFilter`.
/// Used by registration guards to reject bookmark filters in long-lived
/// registered filters/triggers.
pub fn contains_bookmark_qualifier(filter: &ParsedFilter) -> bool {
    match filter {
        ParsedFilter::All | ParsedFilter::None => false,
        ParsedFilter::Qualifiers(qs) => qs
            .iter()
            .any(|q| matches!(q, Qualifier::BookmarkFilter { .. })),
    }
}
```

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test -p logmon-mcp --lib filter::parser`
Expected: all PASS.

- [ ] **Step 5: Add the guard in `handle_filters_add`**

Edit `handle_filters_add` in `src/daemon/rpc_handler.rs`. Insert immediately after extracting the `filter` param string:

```rust
        // Reject bookmark filters in registered (long-lived) filters.
        let parsed = crate::filter::parser::parse_filter(filter)
            .map_err(|e| e.to_string())?;
        if crate::filter::parser::contains_bookmark_qualifier(&parsed) {
            return Err(
                "bookmarks (b>=, b<=) are not allowed in registered filters/triggers — use them only in query tools"
                    .to_string(),
            );
        }
```

- [ ] **Step 6: Add the same guard in `handle_triggers_add`**

In `handle_triggers_add`, after extracting the `filter` param:

```rust
        let parsed = crate::filter::parser::parse_filter(filter)
            .map_err(|e| e.to_string())?;
        if crate::filter::parser::contains_bookmark_qualifier(&parsed) {
            return Err(
                "bookmarks (b>=, b<=) are not allowed in registered filters/triggers — use them only in query tools"
                    .to_string(),
            );
        }
```

- [ ] **Step 7: Build and test**

Run: `cargo build -p logmon-mcp && cargo test -p logmon-mcp --lib`
Expected: clean build, all tests PASS.

- [ ] **Step 8: Commit**

```bash
git add src/filter/parser.rs src/daemon/rpc_handler.rs
git commit -m "feat(daemon): reject bookmark filters in registered filters/triggers"
```

---

## Task 12: Integration test against running daemon

**Files:**

- Create or modify: `tests/bookmarks.rs` (or extend an existing integration test file if one already covers MCP/RPC end-to-end — check `ls tests/` first)

- [ ] **Step 1: Inspect the existing integration test setup**

Run: `ls tests/ && head -40 tests/*.rs | head -100`
Expected: identifies the harness used to spin up a daemon and connect a session. Reuse it. If no harness exists, follow the patterns from the existing tests under `tests/` (the README mentions `test-gelf.sh`, but Rust integration tests should be in `tests/`).

If a harness exists, the rest of this task uses it. If only `test-gelf.sh` exists, write the integration test as a Rust test that spawns the daemon binary the same way `test-gelf.sh` does and communicates via the Unix socket — *do not invent a new harness style*; mirror the existing test pattern exactly.

- [ ] **Step 2: Write the integration test**

Create `tests/bookmarks.rs` (adapting the harness call signatures to whatever the existing tests use — the code below is the *logical* shape, with placeholder helper names that you should rename to match the existing harness):

```rust
//! Integration test for the bookmarks feature.
//! Spins up a daemon, connects a named session, drops bookmarks, and exercises
//! the DSL b>= / b<= operators end-to-end.

use serde_json::{json, Value};

mod common; // assume an existing common harness module under tests/common.rs

#[tokio::test]
async fn bookmarks_end_to_end() {
    let daemon = common::start_daemon().await;
    let session_a = daemon.connect_named("A").await;

    // 1. Send a batch of logs (timestamp T0)
    common::send_log(&daemon, "info", "first batch line 1").await;
    common::send_log(&daemon, "info", "first batch line 2").await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // 2. Set bookmark "before"
    let r = session_a
        .call("bookmarks.add", json!({ "name": "before" }))
        .await
        .unwrap();
    assert_eq!(r["qualified_name"], "A/before");
    assert_eq!(r["replaced"], false);

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // 3. Send a second batch of logs (timestamp T1, after the bookmark)
    common::send_log(&daemon, "warn", "second batch line 1").await;
    common::send_log(&daemon, "error", "second batch line 2").await;

    // 4. Set bookmark "after"
    let r = session_a
        .call("bookmarks.add", json!({ "name": "after" }))
        .await
        .unwrap();
    assert_eq!(r["qualified_name"], "A/after");

    // 5. Query: between bookmarks, expect only the second batch.
    let r = session_a
        .call(
            "logs.recent",
            json!({ "filter": "b>=before, b<=after", "count": 100 }),
        )
        .await
        .unwrap();
    let logs = r["logs"].as_array().unwrap();
    assert_eq!(logs.len(), 2, "expected exactly the second-batch entries");
    let messages: Vec<&str> = logs.iter().map(|l| l["message"].as_str().unwrap()).collect();
    assert!(messages.iter().any(|m| m.contains("second batch line 1")));
    assert!(messages.iter().any(|m| m.contains("second batch line 2")));

    // 6. Cross-session access from a second session
    let session_b = daemon.connect_named("B").await;
    let r = session_b
        .call(
            "logs.recent",
            json!({ "filter": "b>=A/before, b<=A/after", "count": 100 }),
        )
        .await
        .unwrap();
    assert_eq!(r["logs"].as_array().unwrap().len(), 2);

    // 7. Replace flag updates the timestamp
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let r = session_a
        .call("bookmarks.add", json!({ "name": "before", "replace": true }))
        .await
        .unwrap();
    assert_eq!(r["replaced"], true);

    // 8. Resolution failure returns a clear error
    let err = session_a
        .call("logs.recent", json!({ "filter": "b>=ghost", "count": 10 }))
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("bookmark not found"));

    // 9. Registration guard
    let err = session_a
        .call("filters.add", json!({ "filter": "b>=before" }))
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("not allowed in registered filters"));

    // 10. clear_logs followed by list_bookmarks → both bookmarks swept
    session_a.call("logs.clear", json!({})).await.unwrap();
    let r = session_a.call("bookmarks.list", json!({})).await.unwrap();
    assert_eq!(r["count"], 0);

    daemon.shutdown().await;
}
```

- [ ] **Step 3: Run the integration test**

Run: `cargo test -p logmon-mcp --test bookmarks -- --nocapture`
Expected: PASS. If the harness function names differ from `connect_named`, `send_log`, `call`, etc., adapt the calls — the test logic is what matters.

- [ ] **Step 4: Commit**

```bash
git add tests/bookmarks.rs
git commit -m "test(bookmarks): end-to-end integration test"
```

---

## Task 13: Update README.md

**Files:**

- Modify: `README.md`

- [ ] **Step 1: Inspect the existing README structure**

Run: `head -120 README.md` and identify (a) the feature list / tools section and (b) any existing usage example block.

- [ ] **Step 2: Add bookmarks to the tool list**

In the section that enumerates the MCP tools (look for `get_recent_logs`, `add_filter`, etc.), add three entries:

```markdown
- `add_bookmark(name, replace?)` — Set a named timestamp anchor at the current moment. Bookmarks are global, qualified by session name (`session/name`).
- `list_bookmarks(session?)` — List all live bookmarks, newest first. Auto-evicted when both log and span buffers have rolled past their timestamp.
- `remove_bookmark(name)` — Remove a bookmark. Bare name = current session; `session/name` reaches another session.
```

- [ ] **Step 3: Add a "Bookmarks" usage section**

Add this section after the existing filter DSL section (or at the end of the "Usage" section if no filter section exists):

````markdown
### Bookmarks

Bookmarks let you scope queries to a time range without destructively clearing logs. Set one before an operation, another after, and query the range:

```
add_bookmark("before")
# run the operation
add_bookmark("after")
get_recent_logs(filter="b>=before, b<=after, l>=warn")
get_recent_traces(filter="b>=before, b<=after, d>=100")
```

Bookmarks are global across sessions and qualified by the creating session (`session/name`). Bare names in tool calls and DSL expressions resolve to the current session. Bookmarks auto-evict when both the log and span buffers have rolled past their timestamp — they cannot outlive the data they point at.

`b>=` / `b<=` are usable only in query tools, not in registered filters or triggers.
````

- [ ] **Step 4: Verify rendering**

Run: `cat README.md | head -200` and skim the new sections for formatting issues.

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "docs(readme): document bookmarks feature"
```

---

## Task 14: Update skill/logmon.md

**Files:**

- Modify: `skill/logmon.md`

- [ ] **Step 1: Inspect the existing skill structure**

Run: `head -100 skill/logmon.md` to see the section headings and tone.

- [ ] **Step 2: Add a "Bookmarks" section**

Add this section in a logical location (typically after the "Filters" or "Querying logs" section, before "Triggers"):

````markdown
## Bookmarks

Use bookmarks instead of `clear_logs` when you want a clear before/after boundary for an operation but still need the prior history.

**When to reach for bookmarks:**

- Before starting a flaky operation you want to inspect — you can query just that range later instead of wading through everything.
- When comparing two attempts of the same operation — bookmark each attempt's start, then query the two ranges side by side.
- Whenever you'd otherwise reach for `clear_logs` to "see only what happens next" — bookmarks give you the same scoping without losing history.

**Tools:**

- `add_bookmark(name)` — drops a bookmark at the current moment. Pass `replace: true` to overwrite an existing bookmark with the same name.
- `list_bookmarks()` — shows all live bookmarks, newest first. Optional `session` param to filter.
- `remove_bookmark(name)` — removes a bookmark. Bare name = current session; `session/name` for cross-session.

**DSL operators:**

Bookmarks plug into the filter DSL as comparison qualifiers, just like `l>=warn` and `d>=100`:

- `b>=name` — entries at or after the bookmark's timestamp
- `b<=name` — entries at or before
- `b>=other-session/name` — reach into another session's bookmarks

Combine freely:

```
get_recent_logs(filter="b>=before, b<=after, l>=warn")
get_recent_traces(filter="b>=before, d>=100")
get_trace_logs(trace_id="...", filter="b>=before")
```

**Naming and lifetime:**

Bookmarks are global across sessions and stored as `{session_name}/{name}`. Two sessions can both have a bookmark called `before` — they're stored as `A/before` and `B/before` and don't collide. Inside *your* session, a bare `before` means `your-session/before`.

Bookmarks auto-evict when both the log buffer *and* the span buffer have rolled past their timestamp. You cannot end up with a stale bookmark pointing at data that no longer exists.

**Restrictions:**

`b>=` and `b<=` are query-only — they're rejected by `add_filter` and `add_trigger`. Bookmarks are timestamps frozen at creation time, so they don't make sense in long-lived registered filters.
````

- [ ] **Step 3: Verify rendering**

Run: `cat skill/logmon.md | head -250` and skim the new section.

- [ ] **Step 4: Commit**

```bash
git add skill/logmon.md
git commit -m "docs(skill): document bookmarks feature"
```

---

## Task 15: Final verification

**Files:** (none — verification only)

- [ ] **Step 1: Run the entire test suite**

Run: `cargo test -p logmon-mcp`
Expected: all unit and integration tests PASS.

- [ ] **Step 2: Build with warnings as errors to catch dead code**

Run: `RUSTFLAGS="-D warnings" cargo build -p logmon-mcp`
Expected: clean build. If `recent_logs_str` (or any helper) is now unused, decide: remove it (commit separately as "chore: remove unused recent_logs_str") or leave it if it's still referenced somewhere.

- [ ] **Step 3: Smoke-test from a real MCP client**

Manually exercise the new tools end-to-end (e.g. via a connected Claude Code session if logmon-mcp is configured):

1. `add_bookmark("smoke-before")`
2. Generate some log activity in any monitored process.
3. `add_bookmark("smoke-after")`
4. `get_recent_logs(filter="b>=smoke-before, b<=smoke-after")` — confirm the range is what you expect.
5. `list_bookmarks()` — confirm both appear with sane timestamps.
6. `remove_bookmark("smoke-before")` and `remove_bookmark("smoke-after")` — confirm they're gone.

- [ ] **Step 4: Final commit if any cleanups are needed**

```bash
# only if something was changed in steps 2 or 3
git add -p
git commit -m "chore: post-implementation cleanup"
```

---

## Self-review notes

- **Spec coverage:** every section of the spec is mapped to tasks: data model & lifetime → Tasks 1+2+3; DSL extension → Tasks 4+5+6; RPC + MCP tools → Tasks 7+8+9; query handler wiring → Task 10; registration guard → Task 11; testing → Tasks 1–6 (units) and 12 (integration); documentation deliverables → Tasks 13+14.
- **Type consistency:** `BookmarkOp::Gte`/`Lte` used consistently across parser, matcher, and resolver. `qualify()` is the single source of truth for bare-vs-qualified resolution and is shared by `bookmarks.remove` (Task 9) and `resolve_bookmarks` (Task 6). `should_evict` and `sweep` signatures match between Task 2 (definition) and Task 8 (consumption via `sweep_bookmarks`).
- **No placeholders:** every code step contains the actual code; every test step contains the actual assertions; every commit step contains the actual command.
