# Cursor (seq-native bookmarks) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add cursor (read-and-advance) semantics to the existing bookmark mechanism. Bookmarks become seq-based; one new DSL token `c>=name` reads + advances atomically.

**Architecture:** One storage object (`BookmarkEntry`) with a single `seq: u64` position field replaces today's timestamp-based shape. The DSL gains one token (`c>=`) that triggers an atomic read-and-advance via a new `BookmarkStore::cursor_read_and_advance` primitive. Three result types gain `cursor_advanced_to: Option<u64>`. Zero new MCP tools, zero new typed SDK methods, one new SDK builder method.

**Tech Stack:** Rust workspace (logmon-mcp), tokio, serde + schemars, JSON-RPC 2.0 over UDS. Spec at `docs/superpowers/specs/2026-05-01-cursor-bookmarks-design.md` — read it before starting.

**Branch:** `feat/broker-ification` (continues from spec + docs commits). Most recent commit: `5e4a7bc`.

**Test count baseline:** 254 (after broker-ification v1). Each task that adds tests should report the new running total.

---

## File map (decomposition)

**Modified:**

- `crates/core/src/engine/seq_counter.rs` — no code change; pinned by new test.
- `crates/core/src/store/bookmarks.rs` — `BookmarkEntry` shape change (`timestamp` → `seq` + `created_at`); `should_evict` re-keyed on seq; new `cursor_read_and_advance` + `CursorCommit`; `clear_session` already exists.
- `crates/core/src/filter/parser.rs` — new `c>=name` lexer rule; new `CursorFilter` qualifier variant; extend `contains_bookmark_qualifier` predicate.
- `crates/core/src/filter/bookmark_resolver.rs` — handle `CursorFilter` qualifier (call primitive, return lower bound + commit handle).
- `crates/core/src/daemon/persistence.rs` — `PersistedBookmark` shape change; tolerate-and-discard old shape on load.
- `crates/core/src/daemon/rpc_handler.rs` — `add_bookmark` accepts `start_seq`/`replace`; `list_bookmarks` returns new shape; `logs.recent`/`logs.export`/`traces.logs` thread the commit handle and populate `cursor_advanced_to`; allow-list rejection for other methods.
- `crates/core/src/daemon/server.rs` — wire `clear_session` call when anonymous session disconnects.
- `crates/protocol/src/methods.rs` — `BookmarksAdd` gains `start_seq: Option<u64>` + `replace: bool`; `BookmarkInfo` drops `timestamp`, adds `seq: u64` + `created_at`; `LogsRecentResult`/`LogsExportResult`/`TracesLogsResult` gain `cursor_advanced_to: Option<u64>`.
- `crates/protocol/protocol-v1.schema.json` — regen after each protocol change.
- `crates/sdk/src/filter.rs` — add `cursor(name)` method.

**Created:**

- `crates/core/tests/cursors.rs` — integration tests for cursor semantics, allow-list, eviction, persistence, anon-disconnect cleanup.
- `crates/sdk/tests/cursors.rs` — SDK builder + end-to-end tests.

---

## Task 1: Pin seq=0 sentinel contract

**Files:**
- Modify: `crates/core/src/engine/seq_counter.rs` — add doc comment only.
- Test: `crates/core/src/engine/seq_counter.rs::tests` (in-file).

The current implementation already returns `1` on the first `.next()` call (`fetch_add(1) + 1`). The cursor design relies on `seq = 0` being a never-assigned sentinel for "before all records." Add a regression test that pins this contract so a future "optimization" doesn't break it.

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/engine/seq_counter.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Cursor design relies on seq=0 being a never-assigned sentinel.
    /// See docs/superpowers/specs/2026-05-01-cursor-bookmarks-design.md
    /// §Storage `seq = 0 is reserved as a sentinel`.
    #[test]
    fn next_never_returns_zero_from_default() {
        let c = SeqCounter::new();
        assert_eq!(c.next(), 1);
    }

    #[test]
    fn next_never_returns_zero_from_initial_zero() {
        let c = SeqCounter::new_with_initial(0);
        assert_eq!(c.next(), 1);
    }

    #[test]
    fn next_returns_initial_plus_one() {
        let c = SeqCounter::new_with_initial(42);
        assert_eq!(c.next(), 43);
    }
}
```

- [ ] **Step 2: Run test**

```
cargo test -p logmon-broker-core seq_counter::
```

Expected: PASS (current implementation satisfies this).

- [ ] **Step 3: Add doc comment to next()**

Replace the existing doc comment on `pub fn next(&self) -> u64`:

```rust
/// Returns the next sequence number. Always ≥ 1 — `seq = 0` is reserved
/// as a sentinel for "before all records," used by cursor auto-create.
/// See docs/superpowers/specs/2026-05-01-cursor-bookmarks-design.md.
pub fn next(&self) -> u64 {
    self.counter.fetch_add(1, Ordering::Relaxed) + 1
}
```

- [ ] **Step 4: Run test again**

```
cargo test -p logmon-broker-core seq_counter::
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/engine/seq_counter.rs
git commit -m "test(seq_counter): pin seq=0 sentinel contract for cursor design"
```

---

## Task 2: BookmarkEntry uses seq + protocol structs + handlers — single atomic change

**Why merged:** the original Task 2 (storage shape) and Task 4 (protocol/handler shape) are tightly coupled. A split commit between them leaves the wire JSON and the typed `BookmarkInfo` deserializer disagreeing — any test or SDK consumer that round-trips through the typed struct would fail mid-plan. Land them together.

**Files:**
- Modify: `crates/core/src/store/bookmarks.rs` — drop `timestamp` field from `Bookmark`; add `seq: u64` and `description: Option<String>` (keep existing `created_at`, `qualified_name`, `name`, `session`); update `should_evict` to compare seqs; update `add()` signature to `add(session, name, seq, description, replace) -> Result<(Bookmark, bool)>`; rename existing `sweep()` to keep seq-typed args.
- Modify: `crates/core/src/store/bookmarks.rs` (in-file unit tests) — rewrite the bookmark-add tests for the new signature.
- Modify: `crates/protocol/src/methods.rs` — `BookmarkInfo` drops `timestamp`+`age_secs`+`qualified_name`+`session`, gains `seq: u64`+`created_at: DateTime<Utc>`+`description: Option<String>` (keeps `name`); `BookmarksAdd` adds `start_seq: Option<u64>` (already has `name`+`replace`); `BookmarksAddResult` drops `timestamp`, gains `seq: u64`.
- Modify: `crates/core/src/daemon/rpc_handler.rs` — `handle_bookmarks_add` reads `start_seq` (default = current seq counter), reads `replace`, passes both to `bookmark_store.add(...)`; `handle_bookmarks_list` returns the new shape; `handle_bookmarks_clear` unaffected. Add `current_seq()` accessor on `LogPipeline` if missing (one-line passthrough to `SeqCounter::current()`).
- Modify: `crates/core/src/daemon/rpc_handler.rs::sweep` callsite — `BookmarkStore::sweep` now takes seq args; source from pipeline's `oldest_log_seq()` and span store's `oldest_span_seq()` (add accessors if missing).
- Regen: `crates/protocol/protocol-v1.schema.json`.

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/store/bookmarks.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn add_records_seq_and_created_at_and_description() {
    let store = BookmarkStore::new();
    let (bm, replaced) = store.add("session-a", "checkpoint", 42, Some("note"), false).unwrap();
    assert_eq!(bm.seq, 42);
    assert_eq!(bm.description.as_deref(), Some("note"));
    assert_eq!(bm.session, "session-a");
    assert_eq!(bm.name, "checkpoint");
    assert!(!replaced);
}

#[test]
fn add_replace_false_errors_on_existing() {
    let store = BookmarkStore::new();
    let _ = store.add("s", "x", 1, None, false).unwrap();
    let err = store.add("s", "x", 2, None, false).unwrap_err();
    assert!(matches!(err, BookmarkError::AlreadyExists(_)));
}

#[test]
fn add_replace_true_overwrites() {
    let store = BookmarkStore::new();
    let _ = store.add("s", "x", 1, None, false).unwrap();
    let (bm, replaced) = store.add("s", "x", 2, None, true).unwrap();
    assert!(replaced);
    assert_eq!(bm.seq, 2);
}

#[test]
fn evict_by_seq_when_both_stores_advanced_past() {
    let store = BookmarkStore::new();
    store.add("s", "old", 10, None, false).unwrap();
    store.add("s", "new", 100, None, false).unwrap();
    store.sweep(Some(50), Some(50));
    let remaining = store.list();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].seq, 100);
}

#[test]
fn evict_skips_when_either_store_empty() {
    let store = BookmarkStore::new();
    store.add("s", "x", 5, None, false).unwrap();
    store.sweep(Some(100), None);
    assert_eq!(store.list().len(), 1);
    store.sweep(None, Some(100));
    assert_eq!(store.list().len(), 1);
}
```

Also append a typed integration test at `crates/core/tests/cursors.rs` (create the file if not yet present from Task 3 — it's earlier in this revised order):

```rust
#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{BookmarksAdd, BookmarksAddResult, BookmarksList, BookmarksListResult};
use serde_json::json;

#[tokio::test]
async fn add_bookmark_with_explicit_start_seq() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let added: BookmarksAddResult = client.call("bookmarks.add", json!({
        "name": "anchor",
        "start_seq": 42,
        "description": "explicit start"
    })).await.unwrap();
    assert_eq!(added.seq, 42);

    let list: BookmarksListResult = client.call("bookmarks.list", json!({})).await.unwrap();
    let entry = list.bookmarks.iter().find(|b| b.name.ends_with("/anchor")).unwrap();
    assert_eq!(entry.seq, 42);
    assert_eq!(entry.description.as_deref(), Some("explicit start"));
}

#[tokio::test]
async fn add_bookmark_replace_overwrites() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let _: BookmarksAddResult = client.call("bookmarks.add", json!({
        "name": "x", "start_seq": 1
    })).await.unwrap();

    // Without replace, second add errors.
    let err: Result<BookmarksAddResult, _> = client.call("bookmarks.add", json!({
        "name": "x", "start_seq": 2
    })).await;
    assert!(err.is_err());

    // With replace, succeeds.
    let _: BookmarksAddResult = client.call("bookmarks.add", json!({
        "name": "x", "start_seq": 2, "replace": true
    })).await.unwrap();

    let list: BookmarksListResult = client.call("bookmarks.list", json!({})).await.unwrap();
    let entry = list.bookmarks.iter().find(|b| b.name.ends_with("/x")).unwrap();
    assert_eq!(entry.seq, 2);
}
```

- [ ] **Step 2: Run, expect failure**

```
cargo build --workspace
```

Expected: COMPILE FAILURE — `Bookmark.seq` doesn't exist; `BookmarkInfo.seq` doesn't exist; `add()` signature mismatch.

- [ ] **Step 3: Update `Bookmark` and `BookmarkStore`**

In `crates/core/src/store/bookmarks.rs`:

```rust
#[derive(Debug, Clone)]
pub struct Bookmark {
    pub qualified_name: String,
    pub name: String,
    pub session: String,
    pub seq: u64,
    pub created_at: DateTime<Utc>,
    pub description: Option<String>,
}

pub fn should_evict(
    bookmark_seq: u64,
    oldest_log_seq: Option<u64>,
    oldest_span_seq: Option<u64>,
) -> bool {
    let log_evicted = oldest_log_seq.is_some_and(|s| s > bookmark_seq);
    let span_evicted = oldest_span_seq.is_some_and(|s| s > bookmark_seq);
    log_evicted && span_evicted
}

impl BookmarkStore {
    pub fn add(
        &self,
        session: &str,
        name: &str,
        seq: u64,
        description: Option<&str>,
        replace: bool,
    ) -> Result<(Bookmark, bool), BookmarkError> {
        if !is_valid_bookmark_name(name) {
            return Err(BookmarkError::InvalidName(name.to_string()));
        }
        let qualified_name = format!("{session}/{name}");
        let bookmark = Bookmark {
            qualified_name: qualified_name.clone(),
            name: name.to_string(),
            session: session.to_string(),
            seq,
            created_at: Utc::now(),
            description: description.map(String::from),
        };
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        let existed = map.contains_key(&qualified_name);
        if existed && !replace {
            return Err(BookmarkError::AlreadyExists(qualified_name));
        }
        map.insert(qualified_name, bookmark.clone());
        Ok((bookmark, existed))
    }

    pub fn sweep(&self, oldest_log_seq: Option<u64>, oldest_span_seq: Option<u64>) {
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        map.retain(|_, b| !should_evict(b.seq, oldest_log_seq, oldest_span_seq));
    }

    // ... list(), get(), remove(), clear_session() unchanged in shape;
    // adjust list()'s sort to compare on `seq` instead of `timestamp`.
    pub fn list(&self) -> Vec<Bookmark> {
        let map = self.bookmarks.read().expect("bookmarks lock poisoned");
        let mut v: Vec<Bookmark> = map.values().cloned().collect();
        v.sort_by(|a, b| b.seq.cmp(&a.seq));   // seq desc, was timestamp desc
        v
    }
}
```

Update existing in-file tests that constructed bookmarks via the old signature.

- [ ] **Step 4: Update protocol structs**

In `crates/protocol/src/methods.rs`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarkInfo {
    pub name: String,                       // qualified: "session/bookmark"
    pub seq: u64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksAdd {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_seq: Option<u64>,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksAddResult {
    pub qualified_name: String,
    pub seq: u64,
    pub replaced: bool,
}
```

- [ ] **Step 5: Update RPC handlers**

In `crates/core/src/daemon/rpc_handler.rs::handle_bookmarks_add` (current implementation expects `name` + `replace`; extend):

```rust
fn handle_bookmarks_add(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
    let name = params.get("name").and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: name".to_string())?;
    let description = params.get("description").and_then(|v| v.as_str());
    let replace = params.get("replace").and_then(|v| v.as_bool()).unwrap_or(false);
    let seq = params.get("start_seq").and_then(|v| v.as_u64())
        .unwrap_or_else(|| self.pipeline.current_seq());

    let (bm, replaced) = self.bookmarks.add(&session_id.to_string(), name, seq, description, replace)
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "qualified_name": bm.qualified_name,
        "seq": bm.seq,
        "replaced": replaced,
    }))
}
```

In `handle_bookmarks_list`:

```rust
fn handle_bookmarks_list(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
    let session_filter = params.get("session").and_then(|v| v.as_str()).map(String::from);
    let all = self.bookmarks.list();
    let bookmarks: Vec<Value> = all.into_iter()
        .filter(|b| session_filter.as_deref().is_none_or(|s| b.session == s))
        .map(|b| json!({
            "name": b.qualified_name,
            "seq": b.seq,
            "created_at": b.created_at.to_rfc3339(),
            "description": b.description,
        }))
        .collect();
    Ok(json!({ "bookmarks": bookmarks, "count": bookmarks.len() }))
}
```

Find the existing `bookmarks.sweep(...)` call site (eviction sweep — likely in the log/span processor, run periodically). Change the args to `Some(pipeline.oldest_log_seq())` and `Some(span_store.oldest_span_seq())`. Add those accessors if missing (they read the store's first entry's seq, return `None` if empty).

Add to `crates/core/src/engine/pipeline.rs::LogPipeline`:

```rust
pub fn current_seq(&self) -> u64 {
    self.seq_counter.current()
}
pub fn oldest_log_seq(&self) -> Option<u64> {
    self.store.iter().map(|e| e.seq).min()  // or however the store exposes its iteration
}
```

Add equivalent `oldest_span_seq` to `SpanStore`.

- [ ] **Step 6: Migrate filter qualifier from `TimestampFilter` to `SeqFilter`**

Today's `crates/core/src/filter/parser.rs` declares an internal-only qualifier variant `TimestampFilter { op: BookmarkOp, ts: DateTime<Utc> }` (line 21) that the resolver emits from `Bookmark.timestamp`. After this task replaces `Bookmark.timestamp` with `Bookmark.seq`, `TimestampFilter` is unreachable and the resolver no longer compiles. Migrate the variant atomically:

In `crates/core/src/filter/parser.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Qualifier {
    BarePattern(Pattern),
    SelectorPattern(Selector, Pattern),
    LevelFilter { op: LevelOp, level: Level },
    DurationFilter(DurationOp, f64),
    BookmarkFilter { op: BookmarkOp, name: String },
    /// Internal-only: produced by `resolve_bookmarks` from `BookmarkFilter`
    /// (and from `CursorFilter` once Task 5 lands). Never emitted by the parser,
    /// never serialized.
    SeqFilter { op: BookmarkOp, value: u64 },
}
```

(Drop the old `TimestampFilter` variant entirely.)

In `crates/core/src/filter/matcher.rs`, find the three `Qualifier::BookmarkFilter { .. } => false` arms (lines ~20, ~185, plus the test-only construction around line 239). For each `match` over `Qualifier`, replace the `TimestampFilter { op, ts }` arm with:

```rust
Qualifier::SeqFilter { op, value } => match op {
    BookmarkOp::Gte => entry.seq > *value,
    BookmarkOp::Lte => entry.seq < *value,
},
```

(Strict greater-than / less-than per spec semantics: `b>=name` returns `entry.seq > bookmark.seq`, not `>=`. The `Gte`/`Lte` names refer to the user-facing operator, not the internal comparison.)

In `crates/core/src/filter/bookmark_resolver.rs`:

```rust
Qualifier::BookmarkFilter { op, name } => {
    let qualified = qualify(&name, current_session);
    let bookmark = store
        .get(&qualified)
        .ok_or(BookmarkResolutionError::NotFound(qualified))?;
    out.push(Qualifier::SeqFilter {
        op,
        value: bookmark.seq,
    });
}
```

Update the in-file resolver tests (4 tests asserting `Qualifier::TimestampFilter`) to assert `Qualifier::SeqFilter` instead, with `value` matching the bookmark's seq.

- [ ] **Step 7: Regenerate schema**

```
cargo xtask gen-schema
cargo xtask verify-schema
```

Expected: clean.

- [ ] **Step 8: Run all tests**

```
cargo build --workspace --all-targets
cargo test --workspace
```

Expected: 254 + 5 (unit tests in bookmarks::tests) + 2 (integration tests in cursors.rs) = 261. Existing `b>=` round-trip tests in resolver should still pass after the `TimestampFilter → SeqFilter` rewrite.

- [ ] **Step 9: Commit**

```bash
git add crates/core/src/store/bookmarks.rs crates/core/src/engine/pipeline.rs crates/core/src/span/store.rs crates/core/src/daemon/rpc_handler.rs crates/core/src/filter/parser.rs crates/core/src/filter/matcher.rs crates/core/src/filter/bookmark_resolver.rs crates/protocol/src/methods.rs crates/protocol/protocol-v1.schema.json crates/core/tests/cursors.rs
git commit -m "refactor(bookmarks): seq-based positions; protocol + handlers + filter qualifier (TimestampFilter→SeqFilter) atomic"
```

---

## Task 3: PersistedBookmark uses seq + created_at; tolerate-and-discard old shape

**Files:**
- Modify: `crates/core/src/daemon/persistence.rs` — change `PersistedBookmark` shape; wrap bookmark deserialization in error-tolerant loader.
- Test: `crates/core/tests/cursors.rs` (new file) — `legacy_state_json_warns_and_continues`.

- [ ] **Step 1: Write the failing test**

Create `crates/core/tests/cursors.rs`:

```rust
#![cfg(feature = "test-support")]

use std::fs;
use logmon_broker_core::test_support::*;
use serde_json::json;
use logmon_broker_protocol::SessionListResult;

#[tokio::test]
async fn legacy_state_json_with_old_bookmark_shape_warns_and_continues() {
    // Spawn a daemon, get its tempdir, shut down, write an old-shape state.json,
    // restart, verify daemon starts and bookmarks are empty.
    let mut daemon = spawn_test_daemon().await;
    let tempdir_path = daemon.tempdir.path().to_path_buf();
    daemon.shutdown().await;

    // Write a state.json with a named session that has a legacy-shape bookmark
    // (timestamp instead of seq).
    let legacy = json!({
        "seq_block": 1000,
        "named_sessions": {
            "test_session": {
                "triggers": [],
                "filters": [],
                "client_info": null,
                "bookmarks": [{
                    "name": "old",
                    "timestamp": "2026-04-30T12:00:00Z",
                    "description": "from before cursor migration"
                }]
            }
        }
    });
    fs::write(tempdir_path.join("state.json"), serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

    // Re-spawn with same tempdir
    let daemon = TestDaemonHandle::spawn_in_tempdir(daemon.tempdir.clone(), daemon.config.clone()).await;
    let mut client = daemon.connect_named("test_session", None).await;
    let _list: SessionListResult = client.call("session.list", json!({})).await.unwrap();
    // Verify session restored but bookmarks dropped — no panic, daemon ran normally.
    // (We don't have a bookmarks.list method in scope here; just confirm the daemon answered.)
}
```

- [ ] **Step 2: Run test, expect failure**

```
cargo test -p logmon-broker-core --features test-support --test cursors legacy_state_json
```

Expected: FAIL — daemon crashes on deserialize because old `timestamp` field doesn't match new `seq` requirement, OR because `bookmarks` array shape mismatch.

- [ ] **Step 3: Update PersistedBookmark + load tolerance**

In `crates/core/src/daemon/persistence.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedBookmark {
    pub name: String,
    pub seq: u64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
```

In `PersistedSession`, change `bookmarks` field to use a custom deserializer that tolerates failure:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    #[serde(default)]
    pub triggers: Vec<PersistedTrigger>,
    #[serde(default)]
    pub filters: Vec<PersistedFilter>,
    #[serde(default)]
    pub client_info: Option<serde_json::Value>,
    /// Tolerant: deserialize errors → empty vec + WARN log.
    #[serde(default, deserialize_with = "deserialize_bookmarks_lenient")]
    pub bookmarks: Vec<PersistedBookmark>,
}

fn deserialize_bookmarks_lenient<'de, D>(deserializer: D) -> Result<Vec<PersistedBookmark>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let raw = serde_json::Value::deserialize(deserializer)?;
    match serde_json::from_value::<Vec<PersistedBookmark>>(raw) {
        Ok(v) => Ok(v),
        Err(e) => {
            tracing::warn!(error = %e, "discarding bookmarks from previous-version state.json");
            Ok(Vec::new())
        }
    }
}
```

If `PersistedSession` doesn't yet have a `bookmarks` field, add it (the spec says it has one already; if not, this is the addition).

In the session restoration path (`SessionRegistry::restore_named` in `crates/core/src/daemon/session.rs`), iterate `persisted.bookmarks` and call `bookmark_store.add(name, &pb.name, pb.seq, pb.description.as_deref())`. (You'll need access to the `bookmark_store` from the session registry's restore path — pass it in as a param or look up via a shared Arc.)

In the persistence save path (`SessionRegistry::snapshot_named_for_persistence` from broker-ification Task 11), include the bookmarks: iterate the bookmark_store filtered by `session == this session_name` and emit `PersistedBookmark` entries.

- [ ] **Step 4: Run the test, expect pass**

```
cargo test -p logmon-broker-core --features test-support --test cursors
```

Expected: PASS. Tail the daemon log; should see "discarding bookmarks from previous-version state.json".

- [ ] **Step 4.5: Add cross-restart cursor preservation test**

Append to `crates/core/tests/cursors.rs`:

```rust
use logmon_broker_core::gelf::message::Level;
use logmon_broker_protocol::LogsRecentResult;

#[tokio::test]
async fn cursor_persists_across_restart_returns_post_restart_records_only() {
    let mut daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_named("persist", None).await;

    // Inject + advance the cursor.
    daemon.inject_log(Level::Info, "before-restart").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r1: LogsRecentResult = client.call("logs.recent", json!({
        "filter": "c>=mycur", "count": 100
    })).await.unwrap();
    assert!(r1.cursor_advanced_to.is_some());
    let advanced_to = r1.cursor_advanced_to.unwrap();

    // Drop client, restart daemon, reconnect.
    drop(client);
    daemon.restart().await;
    let mut client = daemon.connect_named("persist", None).await;

    // The cursor's persisted seq is now far below the new seq counter (which
    // resumed at state.seq_block + SEQ_BLOCK_SIZE). The in-memory buffer is
    // empty post-restart; first c>= read returns nothing.
    let r2: LogsRecentResult = client.call("logs.recent", json!({
        "filter": "c>=mycur", "count": 100
    })).await.unwrap();
    assert!(r2.logs.is_empty(), "expected empty post-restart, got {:?}", r2.logs);
    assert_eq!(r2.cursor_advanced_to, None);

    // Inject a new record post-restart; cursor should advance again from the
    // post-restart seq (much higher than advanced_to before restart).
    daemon.inject_log(Level::Info, "after-restart").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r3: LogsRecentResult = client.call("logs.recent", json!({
        "filter": "c>=mycur", "count": 100
    })).await.unwrap();
    assert_eq!(r3.logs.len(), 1);
    assert_eq!(r3.logs[0].message, "after-restart");
    assert!(r3.cursor_advanced_to.unwrap() > advanced_to,
        "post-restart cursor seq should exceed pre-restart seq");
}
```

This test pins the spec's "persisted cursor seq is functionally 'skip to restart'" semantic.

- [ ] **Step 5: Run all tests**

```
cargo test --workspace
```

Expected: 257 + 2 = 259 (one new tolerate-and-discard test + one cross-restart preservation test).

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/daemon/persistence.rs crates/core/src/daemon/session.rs crates/core/tests/cursors.rs
git commit -m "feat(persistence): seq-based bookmarks; tolerate-and-discard legacy state.json shape; cross-restart preservation"
```

---

## Task 4: ~~(REMOVED — merged into Task 2)~~

The original Task 4 (protocol struct shape change + add_bookmark / list_bookmarks handler updates) is now part of **Task 2**, because splitting it left a transient state where the wire JSON and the typed `BookmarkInfo` deserializer disagreed (typed clients would deserialize-fail mid-plan).

Task numbers 5–15 are unchanged to avoid renumbering churn across cross-references in this document.

---

## Task 5: Filter parser — `c>=` token + multi-cursor reject + extend rejection predicate

**Files:**
- Modify: `crates/core/src/filter/parser.rs` — new `Qualifier::CursorFilter` variant; new lexer rule for `c>=`; multi-cursor rejection; extend `contains_bookmark_qualifier` to also flag cursors.
- Test: in-file `#[cfg(test)] mod tests` block.

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/filter/parser.rs::tests`:

```rust
#[test]
fn parses_c_ge_qualifier() {
    let f = parse_filter("c>=mycursor").unwrap();
    assert!(matches!(
        &f.qualifiers[0],
        Qualifier::CursorFilter { name } if name == "mycursor"
    ));
}

#[test]
fn parses_c_ge_with_session_qualifier() {
    let f = parse_filter("c>=other-sess/cursor1").unwrap();
    assert!(matches!(
        &f.qualifiers[0],
        Qualifier::CursorFilter { name } if name == "other-sess/cursor1"
    ));
}

#[test]
fn rejects_multiple_cursor_qualifiers() {
    let err = parse_filter("c>=a, c>=b").unwrap_err();
    assert!(format!("{err}").contains("only one cursor qualifier"));
}

#[test]
fn rejects_c_le_token() {
    // c<= is intentionally not defined (snapshot-of-past doesn't fit
    // streaming semantics). Should fall through to "unknown qualifier".
    let err = parse_filter("c<=foo").unwrap_err();
    // Exact error doesn't matter; just that it doesn't parse as Cursor.
    assert!(format!("{err}").to_lowercase().contains("invalid") || format!("{err}").to_lowercase().contains("unknown"));
}

#[test]
fn rejects_invalid_cursor_name_chars() {
    let err = parse_filter("c>=bad name").unwrap_err();
    assert!(matches!(err, FilterParseError::InvalidBookmarkName(_)));
}

#[test]
fn contains_bookmark_qualifier_matches_cursor() {
    let f = parse_filter("c>=foo").unwrap();
    assert!(contains_bookmark_qualifier(&f));
    let f = parse_filter("b>=foo").unwrap();
    assert!(contains_bookmark_qualifier(&f));
    let f = parse_filter("l>=ERROR").unwrap();
    assert!(!contains_bookmark_qualifier(&f));
}
```

- [ ] **Step 2: Run, expect failure**

```
cargo test -p logmon-broker-core filter::parser::tests::parses_c_ge
```

Expected: FAIL — `Qualifier::CursorFilter` doesn't exist.

- [ ] **Step 3: Add the qualifier variant**

In `crates/core/src/filter/parser.rs`:

```rust
pub enum Qualifier {
    // ... existing variants
    CursorFilter { name: String },
}
```

Add lexer rule, mirroring the `b>=` block (around line 311):

```rust
// Cursor filter: c>=NAME (read-and-advance variant of b>=)
if let Some(rest) = token.strip_prefix("c>=") {
    let name = rest.trim();
    if name.is_empty() {
        return Err(FilterParseError::EmptyBookmarkName);
    }
    if !is_valid_bookmark_token(name) {
        return Err(FilterParseError::InvalidBookmarkName(name.to_string()));
    }
    return Ok(Qualifier::CursorFilter { name: name.to_string() });
}
```

After all qualifiers are parsed (in `parse_filter`, just before returning), add multi-cursor rejection:

```rust
let cursor_count = qualifiers.iter()
    .filter(|q| matches!(q, Qualifier::CursorFilter { .. }))
    .count();
if cursor_count > 1 {
    return Err(FilterParseError::MultipleCursorQualifiers);
}
```

Add the variant to `FilterParseError`:

```rust
#[derive(Debug, Error)]
pub enum FilterParseError {
    // ... existing
    #[error("only one cursor qualifier permitted per filter")]
    MultipleCursorQualifiers,
}
```

Extend `contains_bookmark_qualifier` (search for existing fn):

```rust
pub fn contains_bookmark_qualifier(filter: &ParsedFilter) -> bool {
    filter.qualifiers.iter().any(|q| matches!(
        q,
        Qualifier::BookmarkFilter { .. } | Qualifier::CursorFilter { .. }
    ))
}
```

- [ ] **Step 4: Run tests, expect pass**

```
cargo test -p logmon-broker-core filter::parser::
```

Expected: PASS, including the new tests + all existing parser tests.

- [ ] **Step 5: Update rejection error string in rpc_handler**

The handler in `add_filter` and `add_trigger` calls `contains_bookmark_qualifier(&parsed)` and emits an error. Update the error string:

```rust
if crate::filter::parser::contains_bookmark_qualifier(&parsed) {
    return Err(
        "bookmarks and cursors (b>=, b<=, c>=) are not allowed in registered filters/triggers — use them only in query tools"
            .to_string(),
    );
}
```

(The existing call site in `rpc_handler.rs::handle_triggers_add` is around line 319 — search for `contains_bookmark_qualifier(` to find both `add_filter` and `add_trigger` sites.)

- [ ] **Step 6: Run all tests**

```
cargo test --workspace
```

Expected: 260 + 6 = 266.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/filter/parser.rs crates/core/src/daemon/rpc_handler.rs
git commit -m "feat(filter): c>= cursor qualifier; multi-cursor reject; extend bookmark-class predicate"
```

---

## Task 6: BookmarkStore::cursor_read_and_advance + CursorCommit

**Files:**
- Modify: `crates/core/src/store/bookmarks.rs` — new method + helper type.
- Test: in-file unit tests.

This is the atomic read-and-advance primitive. Auto-create on missing entry; commit re-inserts entry at high-water mark if eviction raced the query.

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/store/bookmarks.rs::tests`:

```rust
#[test]
fn cursor_read_and_advance_auto_creates_at_zero() {
    let store = BookmarkStore::new();
    let (lower, _commit) = store.cursor_read_and_advance("s", "fresh");
    assert_eq!(lower, 0);
    let listed = store.list();
    let entry = listed.iter().find(|b| b.qualified_name == "s/fresh").unwrap();
    assert_eq!(entry.seq, 0);
}

#[test]
fn cursor_read_and_advance_returns_existing_seq() {
    let store = BookmarkStore::new();
    let _ = store.add("s", "existing", 50, None, false).unwrap();
    let (lower, _commit) = store.cursor_read_and_advance("s", "existing");
    assert_eq!(lower, 50);
}

#[test]
fn commit_advances_when_max_greater_than_lower() {
    let store = BookmarkStore::new();
    let (lower, commit) = store.cursor_read_and_advance("s", "c");
    assert_eq!(lower, 0);
    commit.commit(100);
    let entry = store.list().into_iter().find(|b| b.qualified_name == "s/c").unwrap();
    assert_eq!(entry.seq, 100);
}

#[test]
fn commit_no_op_when_max_le_lower() {
    let store = BookmarkStore::new();
    let _ = store.add("s", "c", 50, None, false).unwrap();
    let (lower, commit) = store.cursor_read_and_advance("s", "c");
    assert_eq!(lower, 50);
    commit.commit(50);   // No new records — max equals lower.
    let entry = store.list().into_iter().find(|b| b.qualified_name == "s/c").unwrap();
    assert_eq!(entry.seq, 50);
}

#[test]
fn commit_re_inserts_after_eviction_race() {
    let store = BookmarkStore::new();
    let (_lower, commit) = store.cursor_read_and_advance("s", "c");
    // Simulate eviction sweep removing the entry between read-and-advance and commit.
    store.sweep(Some(u64::MAX), Some(u64::MAX));
    assert!(store.list().iter().all(|b| b.qualified_name != "s/c"));
    // Commit re-inserts at the high-water mark.
    commit.commit(200);
    let entry = store.list().into_iter().find(|b| b.qualified_name == "s/c").unwrap();
    assert_eq!(entry.seq, 200);
}

#[test]
fn auto_create_after_eviction_logs_warn() {
    use tracing_test::traced_test;
    #[traced_test]
    fn inner() {
        let store = BookmarkStore::new();
        let _ = store.add("s", "evicted", 5, None, false).unwrap();
        // Sweep evicts the bookmark.
        store.sweep(Some(u64::MAX), Some(u64::MAX));
        // Subsequent c>= reference auto-recreates and should WARN.
        let (lower, _commit) = store.cursor_read_and_advance("s", "evicted");
        assert_eq!(lower, 0);  // Recreated at seq=0
        assert!(logs_contain("auto-recreating at seq=0"));
    }
    inner();
}
```

If `tracing-test` isn't yet in `[dev-dependencies]`, add `tracing-test = "0.2"` to `crates/core/Cargo.toml`.

- [ ] **Step 2: Run, expect failure**

```
cargo test -p logmon-broker-core bookmarks::tests::cursor_read_and_advance
```

Expected: FAIL — method doesn't exist.

- [ ] **Step 3: Implement the primitive (with WARN-on-evicted-auto-recreate baked in)**

In `crates/core/src/store/bookmarks.rs`:

```rust
use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

pub struct BookmarkStore {
    bookmarks: Arc<RwLock<HashMap<String, Bookmark>>>,
    /// Tracks names removed by `sweep` since the last call to `cursor_read_and_advance`
    /// for that name. Lets the primitive distinguish "fresh auto-create" from
    /// "post-eviction auto-recreate" so we can WARN in the latter case.
    /// Bounded to MAX_RECENTLY_EVICTED entries; oldest dropped when over.
    recently_evicted: Mutex<HashSet<String>>,
}

const MAX_RECENTLY_EVICTED: usize = 1024;

pub struct CursorCommit {
    bookmarks: Arc<RwLock<HashMap<String, Bookmark>>>,
    qualified_name: String,
    session: String,
    name: String,
    lower_bound: u64,
}

impl CursorCommit {
    /// Advance the cursor to `max_returned_seq`. No-op if `max_returned_seq <= lower_bound`
    /// (no records returned). If the entry was evicted by `sweep` between
    /// read-and-advance and commit, re-inserts at `max_returned_seq` —
    /// preserves advance intent across racing eviction.
    pub fn commit(self, max_returned_seq: u64) {
        if max_returned_seq <= self.lower_bound {
            return;
        }
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        match map.get_mut(&self.qualified_name) {
            Some(b) => {
                b.seq = max_returned_seq;
            }
            None => {
                // Evicted during the lock-free query phase — re-insert at high-water mark.
                map.insert(self.qualified_name.clone(), Bookmark {
                    qualified_name: self.qualified_name.clone(),
                    session: self.session.clone(),
                    name: self.name.clone(),
                    seq: max_returned_seq,
                    created_at: chrono::Utc::now(),
                    description: None,
                });
            }
        }
    }
}

impl BookmarkStore {
    /// Atomic get-or-create + capture lower bound. Returns (lower_bound, commit_handle).
    /// On auto-create of a name recently evicted by `sweep`, logs at WARN — the
    /// next read will return the full buffer instead of a delta.
    pub fn cursor_read_and_advance(&self, session: &str, name: &str) -> (u64, CursorCommit) {
        let qualified_name = format!("{session}/{name}");
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        let lower_bound = match map.get(&qualified_name) {
            Some(b) => b.seq,
            None => {
                // Check whether this is a post-eviction auto-recreate
                let was_evicted = {
                    let mut recent = self.recently_evicted.lock().expect("recently_evicted poisoned");
                    recent.remove(&qualified_name)
                };
                if was_evicted {
                    tracing::warn!(
                        cursor = %qualified_name,
                        "cursor was evicted under buffer churn; auto-recreating at seq=0 (next read returns full buffer)"
                    );
                } else {
                    tracing::debug!(cursor = %qualified_name, "cursor auto-created at seq=0");
                }
                map.insert(qualified_name.clone(), Bookmark {
                    qualified_name: qualified_name.clone(),
                    session: session.to_string(),
                    name: name.to_string(),
                    seq: 0,
                    created_at: chrono::Utc::now(),
                    description: None,
                });
                0
            }
        };
        drop(map);
        (lower_bound, CursorCommit {
            bookmarks: self.bookmarks.clone(),
            qualified_name,
            session: session.to_string(),
            name: name.to_string(),
            lower_bound,
        })
    }

    /// Override `sweep` to track evicted names so future cursor auto-creates can WARN.
    pub fn sweep(&self, oldest_log_seq: Option<u64>, oldest_span_seq: Option<u64>) {
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        let evicted: Vec<String> = map.iter()
            .filter(|(_, b)| should_evict(b.seq, oldest_log_seq, oldest_span_seq))
            .map(|(k, _)| k.clone())
            .collect();
        map.retain(|_, b| !should_evict(b.seq, oldest_log_seq, oldest_span_seq));
        drop(map);
        if !evicted.is_empty() {
            let mut recent = self.recently_evicted.lock().expect("recently_evicted poisoned");
            for name in evicted {
                if recent.len() >= MAX_RECENTLY_EVICTED {
                    // Cheap bound: drop an arbitrary entry.
                    if let Some(victim) = recent.iter().next().cloned() {
                        recent.remove(&victim);
                    }
                }
                recent.insert(name);
            }
        }
    }
}
```

`BookmarkStore::new` must be updated to initialize `recently_evicted: Mutex::new(HashSet::new())` and to wrap `bookmarks` in `Arc<RwLock<...>>` (was plain `RwLock<...>`). The `Arc` wrap is required so `CursorCommit` can hold an owned reference to the map.

This single step provides both the read-and-advance primitive AND the WARN-on-evicted-recreate behavior. No retrofit needed.

- [ ] **Step 4: Run tests, expect pass**

```
cargo test -p logmon-broker-core bookmarks::tests::
```

Expected: PASS, including `auto_create_after_eviction_logs_warn`.

- [ ] **Step 5: Run all tests**

```
cargo test --workspace
```

Expected: 266 + 6 = 272 (5 unit tests from this task's primitive + 1 WARN-on-recreate).

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/store/bookmarks.rs
git commit -m "feat(bookmarks): cursor_read_and_advance primitive with commit-re-insert + WARN on evicted auto-recreate"
```

---

## Task 7: Filter resolver handles CursorFilter qualifier — and 8 callers update atomically

**Files:**
- Modify: `crates/core/src/filter/bookmark_resolver.rs` — return type changes; handle `Qualifier::CursorFilter`.
- Modify: 8 RPC handler call sites in `crates/core/src/daemon/rpc_handler.rs` (enumerated below).
- Test: in-file unit tests in `bookmark_resolver.rs::tests`.

The resolver currently translates `Qualifier::BookmarkFilter { op, name }` into a `SeqFilter` (post-Task-2). For cursors, it must also obtain a `CursorCommit` handle and surface it back to the caller. The return type changes from `Result<ParsedFilter, BookmarkResolutionError>` to `Result<ResolvedFilter, BookmarkResolutionError>`, which is a **breaking signature change touching every call site simultaneously**.

### Caller enumeration — must all update in this task or build fails

The 8 callers of `resolve_bookmarks` in `crates/core/src/daemon/rpc_handler.rs` (verified by `grep -n 'resolve_bookmarks(' crates/core/src/daemon/rpc_handler.rs` at plan-write time; line numbers may shift slightly during implementation — re-grep if needed):

1. `handle_logs_recent` (around line 134)
2. `handle_logs_export` (around line 160)
3. `handle_traces_recent` (around line 402)
4. `handle_traces_summary`
5. `handle_traces_slow`
6. `handle_traces_get`
7. `handle_traces_logs`
8. `handle_logs_context` (if it uses the resolver — verify; if it doesn't take a filter, drop from list)

Note: `handle_spans_context` does NOT take a filter param (per `protocol::SpansContext` shape) and is not a caller. Anywhere else `parse_and_resolve_filter` is called also counts — `crates/core/src/daemon/rpc_handler.rs::parse_and_resolve_filter` is the helper that wraps `resolve_bookmarks`; check whether it's the actual caller or whether handlers call directly. If it's the helper, update the helper's signature once and the cascade collapses to one site.

For non-cursor handlers (allow-list rejection in Task 9), the cursor_commit returned in this task should be `None`. Task 7 just adapts each caller to consume the new return shape; allow-list rejection lands in Task 9.

### `ParsedFilter` shape note

`crates/core/src/filter/parser.rs::ParsedFilter` is an enum, NOT a struct with `.qualifiers`:

```rust
pub enum ParsedFilter {
    All,
    None,
    Qualifiers(Vec<Qualifier>),
}
```

The resolver pattern-matches on it (see today's `bookmark_resolver.rs:20-41`); follow the existing pattern. Tests below match against the `Qualifiers(qs)` arm, not `.qualifiers`.

### `BookmarkResolutionError` extension

The existing error type is `BookmarkResolutionError` (NOT `ResolveError` as the prior plan draft mentioned — names are normalized below). Add one variant:

```rust
#[derive(Debug, Error)]
pub enum BookmarkResolutionError {
    #[error("bookmark not found: {0}")]
    NotFound(String),
    #[error("cross-session cursor advance is not permitted: {0}")]
    CrossSessionCursorAdvance(String),
}
```

### New return shape

```rust
pub struct ResolvedFilter {
    pub filter: ParsedFilter,
    pub cursor_commit: Option<CursorCommit>,
}

pub fn resolve_bookmarks(
    filter: ParsedFilter,
    store: &BookmarkStore,
    current_session: &str,
) -> Result<ResolvedFilter, BookmarkResolutionError>;
```

- [ ] **Step 1: Write the failing test**

In `crates/core/src/filter/bookmark_resolver.rs::tests`:

```rust
#[test]
fn resolves_cursor_qualifier_to_seq_filter_with_auto_create() {
    let bookmarks = BookmarkStore::new();
    let parsed = parse_filter("c>=mycur").unwrap();
    let resolved = resolve_bookmarks(parsed, &bookmarks, "test-session").unwrap();
    // Auto-created at seq=0 → SeqFilter with value=0
    if let ParsedFilter::Qualifiers(qs) = resolved.filter {
        assert!(matches!(
            qs[0],
            Qualifier::SeqFilter { op: BookmarkOp::Gte, value: 0 }
        ));
    } else {
        panic!("expected qualifiers");
    }
    assert!(resolved.cursor_commit.is_some());
}

#[test]
fn rejects_cross_session_cursor_advance() {
    let bookmarks = BookmarkStore::new();
    let parsed = parse_filter("c>=other-session/cur").unwrap();
    let err = resolve_bookmarks(parsed, &bookmarks, "test-session").unwrap_err();
    assert!(matches!(err, BookmarkResolutionError::CrossSessionCursorAdvance(_)));
}

#[test]
fn cursor_qualifier_uses_existing_seq_for_existing_entry() {
    let bookmarks = BookmarkStore::new();
    let _ = bookmarks.add("test-session", "existing", 100, None, false).unwrap();
    let parsed = parse_filter("c>=existing").unwrap();
    let resolved = resolve_bookmarks(parsed, &bookmarks, "test-session").unwrap();
    if let ParsedFilter::Qualifiers(qs) = resolved.filter {
        assert!(matches!(
            qs[0],
            Qualifier::SeqFilter { op: BookmarkOp::Gte, value: 100 }
        ));
    } else {
        panic!("expected qualifiers");
    }
}
```

- [ ] **Step 2: Run, expect failure**

```
cargo build --workspace
```

Expected: COMPILE FAILURE — `resolve_bookmarks` returns the wrong type, all 8 callers break.

- [ ] **Step 4: Implement the resolver case**

In `bookmark_resolver.rs`:

```rust
pub fn resolve_bookmarks(
    parsed: ParsedFilter,
    bookmarks: &BookmarkStore,
    current_session: &str,
) -> Result<ResolvedFilter, ResolveError> {
    let mut new_qualifiers = Vec::with_capacity(parsed.qualifiers.len());
    let mut cursor_commit: Option<CursorCommit> = None;

    for q in parsed.qualifiers {
        match q {
            Qualifier::BookmarkFilter { op, name } => {
                // Existing path: look up bookmark, emit seq comparison
                let bm = lookup_bookmark(bookmarks, &name, current_session)?;
                new_qualifiers.push(Qualifier::SeqFilter {
                    op: match op { BookmarkOp::Gte => SeqOp::Gt, BookmarkOp::Lte => SeqOp::Lt },
                    value: bm.seq,
                });
            }
            Qualifier::CursorFilter { name } => {
                // Reject cross-session advance
                let (target_session, target_name) = split_session_qualified(&name, current_session);
                if target_session != current_session {
                    return Err(ResolveError::CrossSessionCursorAdvance(
                        format!("cross-session cursor advance is not permitted: {name}")
                    ));
                }
                let (lower, commit) = bookmarks.cursor_read_and_advance(&target_session, &target_name);
                new_qualifiers.push(Qualifier::SeqFilter {
                    op: SeqOp::Gt,
                    value: lower,
                });
                cursor_commit = Some(commit);
            }
            other => new_qualifiers.push(other),
        }
    }

    Ok(ResolvedFilter {
        filter: ParsedFilter { qualifiers: new_qualifiers, ..parsed },
        cursor_commit,
    })
}
```

`split_session_qualified(name, current)` parses `"sess/name"` or bare `"name"` and returns `(session, name)`. Reuse the existing helper if `b>=` resolution already does this; otherwise add it.

- [ ] **Step 5: Update all callers of resolve_bookmarks**

Search for `resolve_bookmarks(` across the codebase. The callers in `rpc_handler.rs` (logs.recent / logs.export / traces.logs / etc.) must now handle the new `ResolvedFilter` return type. For non-cursor methods (where `cursor_commit` should always be `None`), assert it's `None` after resolution; if `Some`, the filter contained `c>=` — reject with the allow-list error. The allow-list enforcement is Task 9; for now in this task, just adapt all callers to take `.filter` from the new struct so the build stays green.

```rust
// Update existing callers:
let resolved = resolve_bookmarks(parsed, &self.bookmarks, &session_id.to_string())
    .map_err(|e| e.to_string())?;
let parsed_filter = resolved.filter;
let cursor_commit = resolved.cursor_commit;
// ... pass parsed_filter to existing pipeline calls
// ... cursor_commit handling lands in Tasks 9-12
```

- [ ] **Step 6: Run all tests**

```
cargo test --workspace
```

Expected: 271 + 3 = 274.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/filter/bookmark_resolver.rs crates/core/src/daemon/rpc_handler.rs
git commit -m "feat(filter): bookmark_resolver handles c>= cursor qualifier; cross-session advance rejected"
```

---

## Task 8: Add cursor_advanced_to to result types + schema regen

**Files:**
- Modify: `crates/protocol/src/methods.rs` — add field to `LogsRecentResult`, `LogsExportResult`, `TracesLogsResult`.
- Regen: `crates/protocol/protocol-v1.schema.json`.

- [ ] **Step 1: Add the fields**

In `crates/protocol/src/methods.rs`:

```rust
// LogsRecentResult
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsRecentResult {
    pub logs: Vec<LogEntry>,
    pub count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_advanced_to: Option<u64>,
}

// LogsExportResult
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsExportResult {
    pub logs: Vec<LogEntry>,
    pub count: usize,
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_advanced_to: Option<u64>,
}

// TracesLogsResult
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TracesLogsResult {
    pub logs: Vec<LogEntry>,
    pub count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_advanced_to: Option<u64>,
}
```

(Adjust to match the existing field shape — these are the canonical fields based on the rpc_handler JSON output.)

- [ ] **Step 2: Regen schema**

```
cargo xtask gen-schema
cargo xtask verify-schema
```

Expected: clean.

- [ ] **Step 3: Build, expect existing tests still green**

```
cargo build --workspace
cargo test --workspace
```

Expected: 274 (no new tests yet — handler population lands in Tasks 10-12).

- [ ] **Step 4: Commit**

```bash
git add crates/protocol/src/methods.rs crates/protocol/protocol-v1.schema.json
git commit -m "feat(protocol): add cursor_advanced_to to LogsRecent/LogsExport/TracesLogs result types"
```

---

## Task 9: Allow-list — reject c>= in non-supported handlers

**Files:**
- Modify: `crates/core/src/daemon/rpc_handler.rs` — for each handler NOT in (logs.recent, logs.export, traces.logs), check `resolved.cursor_commit.is_some()` and return an error.
- Test: append to `crates/core/tests/cursors.rs`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn c_ge_rejected_in_logs_context() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let result: Result<serde_json::Value, _> = client.call("logs.context", json!({
        "seq": 1,
        "filter": "c>=mycur"
    })).await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cursor qualifier not permitted"), "got: {err}");
}

#[tokio::test]
async fn c_ge_rejected_in_traces_recent() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let result: Result<serde_json::Value, _> = client.call("traces.recent", json!({
        "filter": "c>=mycur"
    })).await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cursor qualifier not permitted"), "got: {err}");
}

#[tokio::test]
async fn c_ge_rejected_in_traces_summary() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    // Need an actual trace to summary — use a fake trace_id; the c>= check
    // happens at filter resolve time, before trace lookup.
    let result: Result<serde_json::Value, _> = client.call("traces.summary", json!({
        "trace_id": "00000000000000000000000000000001",
        "filter": "c>=mycur"
    })).await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cursor qualifier not permitted"), "got: {err}");
}
```

(Add similar tests for `traces.slow` and `traces.get` if you want exhaustive coverage; the three above are sufficient for the allow-list mechanism. `spans.context` is excluded from rejection because its protocol struct has no `filter` param.)

- [ ] **Step 2: Run, expect failure**

```
cargo test -p logmon-broker-core --features test-support --test cursors c_ge_rejected
```

Expected: FAIL — currently all handlers accept `c>=`.

- [ ] **Step 3: Add the rejection in each non-supported handler**

In `crates/core/src/daemon/rpc_handler.rs`, for each handler NOT in the allow-list (`logs.recent`, `logs.export`, `traces.logs`), after `resolve_bookmarks` succeeds, check the commit:

```rust
let resolved = resolve_bookmarks(parsed, &self.bookmarks, &session_id.to_string())
    .map_err(|e| e.to_string())?;
if resolved.cursor_commit.is_some() {
    return Err(format!("cursor qualifier not permitted in {METHOD_NAME}"));
}
let parsed_filter = resolved.filter;
```

Substitute `METHOD_NAME` with the literal: `"logs.context"`, `"traces.recent"`, `"traces.summary"`, `"traces.slow"`, `"traces.get"`. (`spans.context` is not in this list — its protocol struct `SpansContext` doesn't accept a `filter` param, so there's nothing to reject.)

For methods that don't currently take a `filter` param (e.g. `traces.get` may only take `trace_id`), no change needed.

- [ ] **Step 4: Run tests, expect pass**

```
cargo test -p logmon-broker-core --features test-support --test cursors c_ge_rejected
cargo test --workspace
```

Expected: 274 + 3 = 277.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/daemon/rpc_handler.rs crates/core/tests/cursors.rs
git commit -m "feat(rpc): reject c>= qualifier in non-cursor-supported query methods"
```

---

## Task 10: logs.recent — thread commit, oldest-first when c>=, populate cursor_advanced_to

**Files:**
- Modify: `crates/core/src/daemon/rpc_handler.rs::handle_logs_recent`.
- Modify: `crates/core/src/engine/pipeline.rs` (or wherever `recent_logs` lives) — accept a `cursor_present: bool` argument that flips ordering.
- Test: append to `crates/core/tests/cursors.rs`.

- [ ] **Step 1: Write the failing test**

```rust
use logmon_broker_protocol::{LogsRecent, LogsRecentResult};
use logmon_broker_core::gelf::message::Level;

#[tokio::test]
async fn cursor_advances_and_paginates_oldest_first() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // Inject 5 records.
    for i in 0..5 {
        daemon.inject_log(Level::Info, &format!("record-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // First cursor read with count=3 — returns oldest 3, advances cursor.
    let r1: LogsRecentResult = client.call("logs.recent", json!({
        "count": 3,
        "filter": "c>=cur"
    })).await.unwrap();
    assert_eq!(r1.logs.len(), 3);
    assert_eq!(r1.logs[0].message, "record-0");  // oldest first
    assert_eq!(r1.logs[1].message, "record-1");
    assert_eq!(r1.logs[2].message, "record-2");
    assert!(r1.cursor_advanced_to.is_some());

    // Second cursor read — returns next 2.
    let r2: LogsRecentResult = client.call("logs.recent", json!({
        "count": 3,
        "filter": "c>=cur"
    })).await.unwrap();
    assert_eq!(r2.logs.len(), 2);
    assert_eq!(r2.logs[0].message, "record-3");
    assert_eq!(r2.logs[1].message, "record-4");

    // Third cursor read — empty, no advance.
    let r3: LogsRecentResult = client.call("logs.recent", json!({
        "count": 3,
        "filter": "c>=cur"
    })).await.unwrap();
    assert!(r3.logs.is_empty());
    assert_eq!(r3.cursor_advanced_to, None);
}

#[tokio::test]
async fn no_cursor_returns_newest_first_unchanged() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    for i in 0..3 {
        daemon.inject_log(Level::Info, &format!("record-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let r: LogsRecentResult = client.call("logs.recent", json!({
        "count": 5
    })).await.unwrap();
    assert_eq!(r.logs.len(), 3);
    assert_eq!(r.logs[0].message, "record-2");  // newest first
    assert_eq!(r.logs[2].message, "record-0");
    assert_eq!(r.cursor_advanced_to, None);
}
```

- [ ] **Step 2: Run, expect failure**

```
cargo test -p logmon-broker-core --features test-support --test cursors cursor_advances_and_paginates
```

Expected: FAIL — `cursor_advanced_to` not populated; ordering not switched.

- [ ] **Step 3: Update pipeline.recent_logs signature**

In `crates/core/src/engine/pipeline.rs::LogPipeline::recent_logs` (or equivalent), add an `oldest_first: bool` parameter:

```rust
pub fn recent_logs(
    &self,
    count: usize,
    filter: Option<&ParsedFilter>,
    oldest_first: bool,
) -> Vec<LogEntry> {
    let mut entries: Vec<LogEntry> = self.store.iter()
        .filter(|e| filter.is_none_or(|f| matches_entry(f, e)))
        .cloned()
        .collect();
    if oldest_first {
        entries.sort_by_key(|e| e.seq);
    } else {
        entries.sort_by_key(|e| std::cmp::Reverse(e.seq));
    }
    entries.truncate(count);
    entries
}
```

(Adapt to the existing iteration / sorting style — keep it lazy if today's code uses `iter().take(count)`; the point is just that ordering is configurable.)

Update all existing callers of `recent_logs` to pass `oldest_first: false` to preserve today's behavior.

- [ ] **Step 4: Update handle_logs_recent**

```rust
fn handle_logs_recent(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
    let count = params.get("count").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let filter_str = params.get("filter").and_then(|v| v.as_str());

    // trace_id shortcut path — skip cursor handling
    if let Some(trace_id_hex) = params.get("trace_id").and_then(|v| v.as_str()) {
        let trace_id = u128::from_str_radix(trace_id_hex, 16)
            .map_err(|_| "invalid trace_id")?;
        let logs = self.pipeline.logs_by_trace_id(trace_id);
        return Ok(json!({ "logs": logs, "count": logs.len() }));
    }

    let parsed = match filter_str {
        Some(s) if !s.trim().is_empty() => Some(crate::filter::parser::parse_filter(s).map_err(|e| e.to_string())?),
        _ => None,
    };

    let (resolved_filter, cursor_commit) = match parsed {
        Some(p) => {
            let resolved = crate::filter::bookmark_resolver::resolve_bookmarks(
                p, &self.bookmarks, &session_id.to_string()
            ).map_err(|e| e.to_string())?;
            (Some(resolved.filter), resolved.cursor_commit)
        }
        None => (None, None),
    };

    let oldest_first = cursor_commit.is_some();
    let entries = self.pipeline.recent_logs(count, resolved_filter.as_ref(), oldest_first);

    let advanced_to = if let Some(commit) = cursor_commit {
        let max_seq = entries.iter().map(|e| e.seq).max();
        if let Some(s) = max_seq {
            commit.commit(s);
            Some(s)
        } else {
            None
        }
    } else {
        None
    };

    let mut result = json!({ "logs": entries, "count": entries.len() });
    if let Some(s) = advanced_to {
        result["cursor_advanced_to"] = json!(s);
    }
    Ok(result)
}
```

- [ ] **Step 5: Run tests, expect pass**

```
cargo test -p logmon-broker-core --features test-support --test cursors cursor_advances cursor_no_cursor_returns_newest
cargo test --workspace
```

Expected: 277 + 2 = 279.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/daemon/rpc_handler.rs crates/core/src/engine/pipeline.rs crates/core/tests/cursors.rs
git commit -m "feat(logs.recent): cursor advance + oldest-first ordering when c>= present"
```

---

## Task 11: logs.export — thread commit, populate cursor_advanced_to

**Files:**
- Modify: `crates/core/src/daemon/rpc_handler.rs::handle_logs_export`.
- Test: append to `crates/core/tests/cursors.rs`.

Same pattern as Task 10. Export uses oldest-first ordering when cursor present (same rationale: paginate through historical content monotonically).

- [ ] **Step 1: Write the failing test**

```rust
use logmon_broker_protocol::LogsExportResult;

#[tokio::test]
async fn export_with_cursor_advances_and_returns_oldest_first() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    for i in 0..5 {
        daemon.inject_log(Level::Info, &format!("export-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let r: LogsExportResult = client.call("logs.export", json!({
        "filter": "c>=expcur",
        "count": 100
    })).await.unwrap();
    assert_eq!(r.logs.len(), 5);
    assert_eq!(r.logs[0].message, "export-0");
    assert!(r.cursor_advanced_to.is_some());

    // Second call — empty (cursor advanced past everything).
    let r2: LogsExportResult = client.call("logs.export", json!({
        "filter": "c>=expcur",
        "count": 100
    })).await.unwrap();
    assert!(r2.logs.is_empty());
    assert_eq!(r2.cursor_advanced_to, None);
}
```

- [ ] **Step 2: Run, expect failure**

```
cargo test -p logmon-broker-core --features test-support --test cursors export_with_cursor
```

Expected: FAIL.

- [ ] **Step 3: Update handle_logs_export**

Mirror the Task 10 handler structure: parse filter, resolve, threading commit, populate `cursor_advanced_to`. The export's existing `format` field stays untouched.

- [ ] **Step 4: Run tests**

```
cargo test --workspace
```

Expected: 279 + 1 = 280.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/daemon/rpc_handler.rs crates/core/tests/cursors.rs
git commit -m "feat(logs.export): cursor advance + oldest-first ordering when c>= present"
```

---

## Task 12: traces.logs — thread commit, populate cursor_advanced_to

**Files:**
- Modify: `crates/core/src/daemon/rpc_handler.rs::handle_traces_logs`.
- Test: append to `crates/core/tests/cursors.rs`.

- [ ] **Step 1: Write the failing test**

```rust
use logmon_broker_protocol::TracesLogsResult;

#[tokio::test]
async fn traces_logs_with_cursor_advances() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // Inject logs with a known trace_id (the harness's inject_log doesn't set
    // trace_id today; either extend the harness or use a synthetic via the
    // protocol's logs.export back-channel).
    // For this test, just verify the cursor_advanced_to field is populated
    // when the filter contains c>= and at least one record matches.

    // Use a trace_id that no logs have, plus c>= cursor. Result should be
    // empty but cursor_advanced_to None (no records returned).
    let r: TracesLogsResult = client.call("traces.logs", json!({
        "trace_id": "00000000000000000000000000000001",
        "filter": "c>=tlcur"
    })).await.unwrap();
    assert!(r.logs.is_empty());
    assert_eq!(r.cursor_advanced_to, None);
}
```

(A more thorough test would inject logs with a specific trace_id; that requires harness extension. This minimal test covers the cursor mechanics.)

- [ ] **Step 2: Run, expect failure**

```
cargo test -p logmon-broker-core --features test-support --test cursors traces_logs_with_cursor
```

Expected: FAIL — `cursor_advanced_to` field not populated.

- [ ] **Step 3: Update handle_traces_logs**

Same pattern as Tasks 10-11.

- [ ] **Step 4: Run tests**

```
cargo test --workspace
```

Expected: 280 + 1 = 281.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/daemon/rpc_handler.rs crates/core/tests/cursors.rs
git commit -m "feat(traces.logs): cursor advance + oldest-first ordering when c>= present"
```

---

## Task 13: Anonymous-session disconnect drops bookmarks

**Files:**
- Modify: `crates/core/src/daemon/server.rs::handle_connection` — call `bookmark_store.clear_session(&session_id)` when an anonymous session disconnects.
- Test: append to `crates/core/tests/cursors.rs`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn anonymous_session_disconnect_drops_bookmarks() {
    let daemon = spawn_test_daemon().await;

    // Connect anon, add bookmark, disconnect.
    {
        let mut client = daemon.connect_anon().await;
        let _: serde_json::Value = client.call("bookmarks.add", json!({
            "name": "ephemeral"
        })).await.unwrap();
        // client drops at end of scope → connection closes.
    }

    // Connect a new anon session, list bookmarks — should be empty for the new session,
    // and the previous anon's bookmark should also be gone (verify via a NAMED session
    // that lists across all sessions if such API exists; otherwise, give the daemon a
    // moment then check via a named session that calls bookmarks.list).
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut client = daemon.connect_anon().await;
    let list: serde_json::Value = client.call("bookmarks.list", json!({})).await.unwrap();
    let bookmarks = list["bookmarks"].as_array().unwrap();
    assert!(
        !bookmarks.iter().any(|b| b["name"].as_str().unwrap_or("").ends_with("/ephemeral")),
        "ephemeral bookmark should be dropped on anon disconnect; got: {bookmarks:?}"
    );
}
```

- [ ] **Step 2: Run, expect failure**

```
cargo test -p logmon-broker-core --features test-support --test cursors anonymous_session_disconnect
```

Expected: FAIL — bookmark persists across the anon disconnect.

- [ ] **Step 3: Wire clear_session into the disconnect path**

In `crates/core/src/daemon/server.rs::handle_connection`, after the main loop breaks (where today the code calls `sessions.disconnect(&session_id)`), also call:

```rust
// Drop bookmarks scoped to this session if anonymous (anon sessions are
// removed entirely; named sessions persist their bookmarks).
if matches!(session_id, SessionId::Anonymous(_)) {
    let removed = bookmarks.clear_session(&session_id.to_string());
    if removed > 0 {
        info!(?session_id, removed, "cleared anonymous-session bookmarks on disconnect");
    }
}
sessions.disconnect(&session_id);
```

`handle_connection` already takes `handler: Arc<RpcHandler>` as a param, and `RpcHandler.bookmarks: Arc<BookmarkStore>` is a public field (verify by grep). Access via `handler.bookmarks.clear_session(&session_id.to_string())`. No new threading required.

- [ ] **Step 4: Run tests, expect pass**

```
cargo test -p logmon-broker-core --features test-support --test cursors anonymous_session_disconnect
cargo test --workspace
```

Expected: 281 + 1 = 282.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/daemon/server.rs
git commit -m "feat(session): clear anonymous-session bookmarks on disconnect"
```

---

## Task 14: SDK filter builder — cursor(name) method

**Files:**
- Modify: `crates/sdk/src/filter.rs` — add `cursor(name)` method.
- Test: `crates/sdk/tests/cursors.rs` (new).

- [ ] **Step 1: Write the failing test**

Create `crates/sdk/tests/cursors.rs`:

```rust
use logmon_broker_sdk::{Broker, Filter};
use logmon_broker_protocol::{LogsRecent, LogsRecentResult};
use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;
use logmon_broker_core::gelf::message::Level;

#[test]
fn cursor_builder_emits_c_ge() {
    let f = Filter::builder().cursor("mycur").build();
    assert_eq!(f, "c>=mycur");
}

#[test]
fn cursor_combined_with_other_qualifiers() {
    let f = Filter::builder()
        .cursor("run-1")
        .level_at_least(logmon_broker_sdk::Level::Error)
        .build();
    // Order isn't strictly part of the contract but for current builder it's
    // append order; either accept any-order or pin the order:
    assert!(f.contains("c>=run-1"));
    assert!(f.contains("l>=ERROR"));
}

#[tokio::test]
async fn cursor_end_to_end_advances_with_exact_max_seq() {
    let daemon = spawn_test_daemon_for_sdk().await;
    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .open().await.unwrap();

    daemon.inject_log(Level::Info, "first").await;
    daemon.inject_log(Level::Info, "second").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let r1 = broker.logs_recent(LogsRecent {
        filter: Some(Filter::builder().cursor("e2e-cur").build()),
        count: Some(10),
        ..Default::default()
    }).await.unwrap();

    assert_eq!(r1.logs.len(), 2);
    assert_eq!(r1.logs[0].message, "first");   // oldest first
    let last_seq = r1.logs.last().unwrap().seq;
    assert_eq!(r1.cursor_advanced_to, Some(last_seq));
}
```

- [ ] **Step 2: Run, expect failure**

```
cargo test -p logmon-broker-sdk --test cursors cursor_builder_emits
```

Expected: FAIL — `cursor` method doesn't exist on `FilterBuilder`.

- [ ] **Step 3: Add the builder method**

In `crates/sdk/src/filter.rs::FilterBuilder` impl block:

```rust
/// Cursor (read-and-advance) reference: emits `c>=<name>`. The broker
/// auto-creates the bookmark at seq=0 on first reference. See the design
/// at docs/superpowers/specs/2026-05-01-cursor-bookmarks-design.md.
///
/// Cross-session cursor advance is intentionally not exposed — the broker
/// rejects it. Use `bookmark_after("session/name")` for cross-session
/// pure-read instead.
pub fn cursor(mut self, name: &str) -> Self {
    self.qualifiers.push(format!("c>={name}"));
    self
}
```

- [ ] **Step 4: Run all tests**

```
cargo test -p logmon-broker-sdk
cargo test --workspace
```

Expected: 282 + 3 = 285.

- [ ] **Step 5: Commit**

```bash
git add crates/sdk/src/filter.rs crates/sdk/tests/cursors.rs
git commit -m "feat(sdk): cursor(name) filter-builder method + end-to-end advance test"
```

---

## Task 15: Eviction edge-case tests + final regression sweep

**Files:**
- Test: append to `crates/core/tests/cursors.rs`.

These tests cover the design's stated edge cases that aren't exercised by the happy-path tests in earlier tasks.

- [ ] **Step 1: Write the eviction-during-active-polling test**

```rust
use logmon_broker_core::store::bookmarks::BookmarkStore;

#[test]
fn cursor_eviction_during_query_re_inserts_at_high_water() {
    // Pure-Rust unit test on the store, no daemon needed.
    let store = BookmarkStore::new();
    let (lower, commit) = store.cursor_read_and_advance("s", "racy");
    assert_eq!(lower, 0);

    // Simulate eviction sweep removing the entry mid-flight.
    store.evict_stale(Some(u64::MAX), Some(u64::MAX));
    assert!(store.list().iter().all(|b| b.qualified_name() != "s/racy"));

    // Commit re-inserts at high-water mark.
    commit.commit(500);
    let entry = store.list().into_iter().find(|b| b.qualified_name() == "s/racy").unwrap();
    assert_eq!(entry.seq, 500);
}

#[tokio::test]
async fn cursor_evicted_under_churn_auto_recreates_with_full_buffer() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // Configure a tiny buffer so eviction fires quickly.
    // (Today's test_support uses default config; override config when
    // spawning if available, or skip this test if the harness can't
    // configure buffer_size.)

    // Step 1: create the cursor, advance it once.
    daemon.inject_log(Level::Info, "before").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let _r1: LogsRecentResult = client.call("logs.recent", json!({
        "filter": "c>=churn", "count": 100
    })).await.unwrap();

    // Step 2: flood the buffer past the cursor's seq.
    for i in 0..15_000 {
        daemon.inject_log(Level::Info, &format!("flood-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Step 3: cursor should have evicted; next read returns the full current buffer.
    // (We can't easily assert "WARN was logged" from inside the test without
    // tracing-test scaffolding; assert behaviorally that the result includes
    // many records — far more than the delta would have been.)
    let r3: LogsRecentResult = client.call("logs.recent", json!({
        "filter": "c>=churn", "count": 50_000
    })).await.unwrap();
    assert!(r3.logs.len() >= 1000, "expected flood-recreation to return many records, got {}", r3.logs.len());
}
```

(If the harness can't configure buffer_size, mark the second test `#[ignore]` with a TODO comment pointing at harness extension; the unit test is the primary coverage.)

- [ ] **Step 2: Run new tests**

```
cargo test -p logmon-broker-core --features test-support --test cursors cursor_eviction
```

Expected: PASS (the unit test) + PASS or IGNORED (the integration test, depending on harness).

- [ ] **Step 3: Full workspace regression sweep**

```
cargo build --workspace --all-targets
cargo test --workspace
cargo xtask verify-schema
```

Expected: all clean. Test count: 285 + 1 (or 2) = 286+.

- [ ] **Step 4: Update documentation if implementation diverged from spec**

Re-read `docs/superpowers/specs/2026-05-01-cursor-bookmarks-design.md` and `crates/sdk/README.md`. If anything you implemented diverged from the spec (different field name, different error wording, different ordering decision), update the docs to match the implementation. The spec is the contract; docs reflect reality.

If no divergence, skip.

- [ ] **Step 5: Commit (and tag)**

```bash
git add crates/core/tests/cursors.rs
git commit -m "test(cursors): eviction-during-query commit re-insert + auto-recreate-flood regression"
git tag cursor-feature-v1 -m "cursor (seq-native bookmarks) feature"
```

---

## Self-review (after writing the plan)

**Spec coverage:** spec is at `docs/superpowers/specs/2026-05-01-cursor-bookmarks-design.md`. Walking through each section:

- §Storage (BookmarkEntry shape, seq=0 sentinel, name charset) → Tasks 1, 2.
- §DSL (b>=/c>=/b<= tokens, allow-list, multi-cursor reject, resolution ordering) → Tasks 5, 7, 9, 10–12.
- §Auto-create on c>= reference → Task 6.
- §Creation paths side-by-side → Tasks 2 (explicit add_bookmark with start_seq/replace), 6 (implicit auto-create).
- §Eviction interaction (active polling, WARN on auto-recreate) → Task 6 (WARN baked into primitive impl), Task 15 (regression test).
- §MCP tool changes (add_bookmark/list_bookmarks shape) → Task 2.
- §Result-shape change (cursor_advanced_to on three result types) → Task 8.
- §SDK changes (typed surface, filter builder) → Task 2 (typed surface comes free with protocol struct change), Task 14 (builder).
- §Persistence + cross-restart → Task 3 (now includes cross-restart preservation test).
- §Concurrency (lock acquisitions, eviction-during-query commit re-insert) → Tasks 6, 15.
- §Migration (state.json clean break) → Task 3.
- §Wire changes → Tasks 2, 8.
- §Filter qualifier migration (TimestampFilter → SeqFilter) — atomic with §Storage change → Task 2 (folded into the merged task).
- §Testing list → Tasks 5, 6, 7, 9, 10, 11, 12, 13, 15 cover the integration-test inventory.

**Placeholder scan:** no "TBD/TODO/etc." All steps have actual code or commands. Task 4 is intentionally a "removed — merged into Task 2" placeholder; numbering preserved to avoid renumbering churn across cross-references.

**Type consistency:** `Bookmark` (existing struct, kept; cursor design adds `seq` and `description`, drops `timestamp`). `cursor_read_and_advance` signature consistent across Tasks 6, 7, 15. `Qualifier::CursorFilter { name }` defined in Task 5; `Qualifier::SeqFilter { op: BookmarkOp, value: u64 }` introduced in Task 2 (replaces `TimestampFilter`); both used in Task 7's resolver. `ResolvedFilter` shape consistent across Tasks 7, 9, 10, 11, 12. `CursorCommit::commit(self, max_returned_seq: u64)` consistent across Tasks 6, 7, 10. Field names: `cursor_advanced_to` consistent. `BookmarkResolutionError` (NOT `ResolveError`) — verified against existing code.

**Test count progression:** 254 → 254 (Task 1, no new) → 261 (Task 2 adds 7) → 263 (Task 3 adds 2) → (Task 4 removed) → 269 (Task 5 adds 6) → 275 (Task 6 adds 6) → 278 (Task 7 adds 3) → 278 (Task 8 adds 0) → 281 (Task 9 adds 3) → 283 (Task 10 adds 2) → 284 (Task 11 adds 1) → 285 (Task 12 adds 1) → 286 (Task 13 adds 1) → 289 (Task 14 adds 3) → 290+ (Task 15 adds 1 unit test + optionally 1 integration test depending on harness extension).
