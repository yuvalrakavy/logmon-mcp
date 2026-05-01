# Cursors via seq-native bookmarks — design

**Date:** 2026-05-01
**Status:** approved (pending user review of this document)
**Branch target:** `feat/broker-ification` (or a follow-up branch)

## Summary

Add **cursor** semantics to the existing bookmark mechanism by replacing the bookmark's timestamp position with a seq position and introducing one new DSL token (`c>=name`) that reads-and-advances. Bookmarks become a single concept with two interaction patterns:

- `b>=name` — pure read, today's behavior, returns matching records strictly after the bookmark's seq.
- `c>=name` — read-and-advance, atomically updates the bookmark to the max seq returned by the query. Auto-creates the bookmark at `seq = 0` if it doesn't exist.

The MCP tool surface gains zero new tools. The schema changes are minimal: one renamed field (`timestamp → seq`) and two optional params on `add_bookmark`. The new DSL token is one lexer rule.

## Motivation

Surfaced during the store-test integration design (Spec B handoff): tests want to poll the broker for "what's new since I last checked" without resetting state or threading checkpoint values through their own code. Two concrete patterns:

- **Per-test single-shot.** At end of test, ask "give me everything from my run." Conceptually a bookmark + scoped filter.
- **Per-step multi-poll.** Within a test, several "what's new" reads. Each poll returns the delta since the previous poll.

Today's mechanism handles the first via bookmarks (`b>=name` after `add_bookmark(name)` at test start) but the second requires the test code to track a moving boundary itself. Cursors close that gap.

A second motivation: the broker's `SeqCounter` is shared between log and span stores and gives a globally unique, monotonic, dense u64 per record. Bookmarks based on `entry.timestamp` are fuzzy at boundaries (multiple records per millisecond) and require an "+ ε" fudge for cursor-style advance. Switching the bookmark position to seq eliminates this entirely and unifies the storage primitive.

## Design

### Storage

```rust
// crates/core/src/store/bookmarks.rs
pub struct BookmarkEntry {
    pub session: String,         // qualifying session for the bookmark name
    pub name: String,
    pub seq: u64,                // single position field — points into the shared SeqCounter
    pub created_at: DateTime<Utc>,
    pub description: Option<String>,
}
```

One position field. No discriminator, no enum. The `created_at` is purely informational (human display in `list_bookmarks`); the `seq` is the only field used for filtering.

`BookmarkStore` keeps the same `Mutex<HashMap<(String, String), BookmarkEntry>>` shape as today.

### DSL

| Token | Operation |
|---|---|
| `b>=name` | Filter records to `entry.seq > bookmark.seq`. Pure read. Errors if `name` is unknown. |
| `c>=name` | Same filter, then atomically update `bookmark.seq = max(returned.seq)` after the query completes. Auto-creates the bookmark at `seq = 0` if `name` is unknown. |
| `b<=name` | Filter records to `entry.seq < bookmark.seq`. Pure read. Errors if `name` is unknown. (Existing token, semantics unchanged modulo position type.) |

Both `b>=` and `c>=` accept cross-session reach via `other-session/name`:

- `b>=other/name` — allowed, read-only.
- `c>=other/name` — **rejected at parse-resolve time** with `"cross-session cursor advance is not permitted"`. Mutating another session's bookmark from outside is an attractive footgun, never useful in practice.

Both tokens are rejected by `add_filter` and `add_trigger` (existing restriction for `b>=`/`b<=`); cursor positions don't make sense in long-lived registered filters.

The token's behavior is determined entirely by which token is used. The same bookmark can be referenced via either token by the same session — the bookmark itself has no "this is a cursor" flag.

#### Where `c>=` is permitted

`c>=` requires the query result to consist of seq-anchored records so the advance value is well-defined. Allowed query methods:

- `logs.recent`
- `logs.export`
- `traces.logs`

Rejected at parse-resolve time (`"cursor qualifier not permitted in <method>"`):

- `logs.context` — the result is anchored on a caller-supplied seq; cursor semantics don't fit.
- `traces.recent`, `traces.summary`, `traces.slow` — results are aggregates (`TraceSummary`), not seq-anchored. Callers wanting to tail traces incrementally should use `traces.logs` with `c>=name, tr=...` for per-trace log streaming.
- `traces.get`, `spans.context` — single-record / context-around-anchor queries, no streaming semantics.

Bookmark *read* qualifiers (`b>=`, `b<=`) are permitted in all query methods unchanged; this restriction is specific to `c>=` because of the advance step.

#### One cursor per filter

A filter may contain at most one `c>=` qualifier. Multiple cursor qualifiers (`c>=foo, c>=bar`) are rejected at parse-resolve time with `"only one cursor qualifier permitted per filter"` — both would advance to the same `max(returned.seq)`, defeating the purpose of having two distinct cursors. Bookmark *read* qualifiers can appear any number of times alongside a `c>=` (e.g. `c>=run, b>=before-deploy`).

#### Filter resolution ordering

To ensure a failed query never leaves a half-created cursor behind, the resolver runs in this order:

1. Parse the filter string (lexer + syntax check).
2. Validate the filter against the query method (allow-list above; reject `c>=` for unsupported methods; reject multi-cursor).
3. Resolve cursor and bookmark references — auto-create cursor if missing, capture lower bound, obtain advance commit handle.
4. Execute the store query.
5. Compute `max(returned.seq)`, commit the cursor advance.

A filter that fails at step 1 or 2 never touches the bookmark store. Auto-creation is a side effect of a syntactically and semantically valid `c>=` reference, never of a malformed query.

### Auto-create on `c>=` reference

When the filter resolver encounters `c>=name` and no bookmark exists for `(current_session, name)`:

1. Insert a new `BookmarkEntry { seq: 0, created_at: now(), description: None, ... }`. Initial `seq = 0` means "before all currently-stored records" — first read with the new cursor returns everything matching the rest of the filter.
2. Apply the rest of the filter; return matching records.
3. Atomically advance `bookmark.seq = max(returned.seq)` (no-op if no records returned).

The create + read + advance happen under a single `BookmarkStore` lock acquisition so two concurrent `c>=name` calls don't race on insertion.

If the user wants different initial-position semantics (e.g. "ignore historical buffer content, only stream new records"), they call `add_bookmark(name)` before the first `c>=name` — `add_bookmark` defaults `start_seq` to the current seq counter (see "Creation paths" below). This is the only reason to call `add_bookmark` explicitly when using cursors.

#### Creation paths — side by side

The two creation paths have intentionally different defaults. Both are correct; pick by intent.

| Path | Default `seq` | First read returns | Use when |
|---|---|---|---|
| Explicit: `add_bookmark(name)` (no `start_seq`) | current seq counter value | only records that arrive *after* this call | you want "stream from now" — the polling test wants only its own work, not historical noise |
| Implicit: `c>=name` on missing name | 0 | all records currently in the buffer + everything after | you forgot to set up, or you genuinely want everything currently retained |

To get "stream from now" via the implicit path, simply call `add_bookmark(name)` first; the subsequent `c>=name` finds the bookmark already at current-seq and behaves accordingly.

#### Eviction interaction (active polling)

Cursors are subject to the same eviction rule as bookmarks: a cursor evicts when its `seq` is older than the oldest record in *both* the log and span stores. For an active poller this normally never fires — each `c>=name` advance moves the seq forward as new records arrive.

But under high churn, an idle cursor's seq can fall behind the buffer's tail. When eviction fires and the next `c>=name` reference auto-recreates the cursor at `seq = 0`, the next read returns the entire current buffer rather than a delta. **This is the documented behavior**, not a bug.

The broker logs at WARN when an evicted cursor name is auto-recreated by `c>=`, so a poller that observes a sudden buffer-flood has a server-side signal to correlate. Callers concerned about this case should bump `buffer_size` so the cursor doesn't outpace ingestion.

### MCP tool changes

Three of four bookmark tools change shape; no new tools.

| Tool | Change |
|---|---|
| `add_bookmark` | Optional `start_seq: u64` (default = current seq counter). Optional `replace: bool` (default false). On existing name with `replace=false`, errors with `"bookmark exists; pass replace=true to overwrite"`. With `replace=true`, overwrites unconditionally — note that this is destructive against an actively-polled cursor: the cursor snaps to `start_seq` and any unread records below the new position are skipped on the next `c>=` read. |
| `list_bookmarks` | Result entries return `seq: u64`, `created_at: <ISO 8601 string>`, `name`, `description` (optional). The legacy `timestamp` field is removed. |
| `remove_bookmark` | Unchanged. |
| `clear_bookmarks` | Unchanged. |

`add_bookmark` without `start_seq` and `replace` is wire-compatible with today's call sites (the new params are optional with serde defaults).

### Result-shape change

Three result types — exactly the methods that accept `c>=` per the allow-list above — gain `cursor_advanced_to: Option<u64>`:

- `LogsRecentResult`
- `LogsExportResult`
- `TracesLogsResult`

Populated as `Some(new_seq)` when the query's filter included a `c>=...` qualifier AND at least one record matched (the cursor actually advanced). `None` in two cases: (a) filter contained no `c>=...` qualifier; (b) `c>=...` was present but no records matched, so the cursor is unchanged. Callers that need to know "what's my cursor seq right now regardless" should call `list_bookmarks`.

### SDK changes

**Typed methods:** `BookmarksAdd` gains `start_seq: Option<u64>` and `replace: bool`. `BookmarkInfo` (the `list_bookmarks` element type) gains `seq: u64` and `created_at: DateTime<Utc>`, drops `timestamp`. The three query result types listed above (`LogsRecentResult`, `LogsExportResult`, `TracesLogsResult`) gain `cursor_advanced_to: Option<u64>`. No new method.

**Filter builder:** one new method.

```rust
impl FilterBuilder {
    /// Read-and-advance: emits "c>=<name>". Auto-creates the bookmark at
    /// seq=0 server-side on first reference. Cross-session reach via
    /// `Filter::builder().cursor_in("other-sess", "name")` is rejected at
    /// the broker — only the owning session can advance.
    pub fn cursor(mut self, name: &str) -> Self {
        self.qualifiers.push(format!("c>={name}"));
        self
    }

    pub fn cursor_in(mut self, session: &str, name: &str) -> Self {
        self.qualifiers.push(format!("c>={session}/{name}"));
        self
    }
}
```

The existing `bookmark_after(name)` and `bookmark_before(name)` are unchanged.

### Persistence and eviction

**Persistence.** Named-session state.json keeps its existing `bookmarks: Vec<PersistedBookmark>` shape, with `PersistedBookmark { name, seq, created_at, description }`. Anonymous-session bookmarks are not persisted (today's behavior).

Note on cross-restart cursor behavior: when the daemon restarts, the in-memory log and span buffers are empty and the seq counter resumes at `state.seq_block + SEQ_BLOCK_SIZE`. A persisted cursor's seq is therefore far below any in-buffer record, so the first post-restart `c>=name` returns nothing (the buffer is empty), then deltas as new records arrive. Persisted cursor seq is functionally "skip to restart" — useful for the rare case where a long-running named session resumes its polling after a daemon bounce, but not a substitute for in-memory state across restart. Callers wanting per-test isolation should use unique cursor names per run regardless of session persistence.

**Eviction.** A bookmark is evictable when both stores have rolled past `bookmark.seq`:

```
bookmark.seq < min(log_store.oldest_seq, span_store.oldest_seq)
```

Same rule as today's bookmark eviction, just expressed in seq instead of timestamp. Active cursors stay alive naturally because each `c>=` advance moves `seq` forward as new records arrive.

### Concurrency

`BookmarkStore` exposes one new method:

```rust
impl BookmarkStore {
    /// Atomically: get-or-create the bookmark, capture its seq, then advance
    /// it to `max(returned.seq)` after the caller has computed the result.
    /// Returns the seq the caller should use as the lower bound, plus a
    /// closure to call with the max returned seq.
    pub fn cursor_read_and_advance(
        &self,
        session: &str,
        name: &str,
    ) -> (u64 /* lower bound */, CursorCommit);
}

pub struct CursorCommit { /* holds Arc<...> internally */ }
impl CursorCommit {
    pub fn commit(self, max_returned_seq: u64);  // no-op if max < lower_bound (no records)
}
```

The query path:
1. Parse filter, find `c>=name`. Call `cursor_read_and_advance(session, name)` → `(lower_bound, commit_handle)`.
2. Execute the query with `entry.seq > lower_bound` plus the rest of the filter.
3. Compute `cursor_advanced_to = entries.iter().map(|e| e.seq).max()`.
4. Call `commit_handle.commit(cursor_advanced_to)` (a no-op if no records returned).
5. Return result with `cursor_advanced_to` populated.

The lock is held only during the get-or-create phase and the advance commit, not during the actual store query — so cursor operations don't block other sessions' work.

### Migration

None. The new broker version manifests as a fresh deployment; no prior `state.json` is loaded across the schema change. Old bookmarks vanish on first start of the new binary; users re-create what they need.

A one-line CHANGELOG note: `b>=name` filtering is now seq-based rather than timestamp-based; for live ingestion this is invisible, divergence only manifests under bulk-replay or clock-skew workloads.

## Wire changes

**Protocol crate (`crates/protocol/src/methods.rs`):**

- `BookmarkInfo`: drop `timestamp`, add `seq: u64`, add `created_at: chrono::DateTime<Utc>` (serialized as ISO 8601).
- `BookmarksAdd`: add `start_seq: Option<u64>`, add `replace: bool` (with `#[serde(default)]`).
- `LogsRecentResult`, `LogsExportResult`, `TracesLogsResult`: add `cursor_advanced_to: Option<u64>` (with `#[serde(default, skip_serializing_if = "Option::is_none")]`). `TracesRecentResult` and other `traces.*` / `spans.*` / `logs.context` result types do NOT gain this field — those methods reject `c>=` per the allow-list.

**Schema regen** via `cargo xtask gen-schema` after these changes; `cargo xtask verify-schema` clean.

**Filter parser (`crates/core/src/filter/parser.rs`):** one new lexer rule recognizing `c>=` and emitting a `Cursor` qualifier variant. Filter type gains `Pattern::Cursor { name: String }` (or equivalent). Bookmark resolver in `crates/core/src/filter/bookmark_resolver.rs` extends to handle Cursor qualifiers — does the `cursor_read_and_advance` dance, returns a resolved seq filter plus the commit handle.

**RPC handlers (`crates/core/src/daemon/rpc_handler.rs`):** the three cursor-driven query handlers (`logs.recent`, `logs.export`, `traces.logs`) thread the commit handle through to the result construction site so `cursor_advanced_to` can be set after entries are computed. All other query handlers add a parse-resolve check that rejects filters containing a `c>=` qualifier with `"cursor qualifier not permitted in <method>"`.

## SDK changes

Already covered above. One new builder method, three protocol struct extensions reflected in typed result types automatically. No new typed method on `Broker` since the cursor API piggybacks on `bookmarks_add` / `bookmarks_list` / `bookmarks_remove` / `bookmarks_clear`.

## Testing

**Unit (`crates/core/src/store/bookmarks.rs::tests`):**
- `cursor_read_and_advance` get-or-create on missing name.
- `cursor_read_and_advance` lower bound = current seq for existing entries.
- `commit(seq)` advances when seq > lower bound; no-op when ≤.
- Concurrent `cursor_read_and_advance` calls serialize correctly (insert-then-read by both, second sees the first's insertion).
- Eviction by seq vs both-stores-oldest.

**Filter parser unit tests:**
- `c>=name` parses to expected qualifier.
- `c>=other-sess/name` parses with cross-session qualification.
- `c>=name` rejected by `add_filter` / `add_trigger` filter validators.
- `c<=name` rejected at parse time (not a defined token).

**Integration (`crates/core/tests/cursors.rs`):**
- `auto_create_returns_everything_then_advances` — first `c>=fresh-name` returns all matching records in buffer, second returns delta.
- `cursor_with_other_filter` — `c>=name, l>=ERROR` returns only error-level deltas; cursor advance reflects max seq of *returned* records (not max seq of unfiltered records).
- `cursor_per_session` — bare `c>=foo` from session A and session B don't collide.
- `cursor_persists_across_restart` — named session, `c>=name`, restart, `c>=name` again → second call returns only what arrived during restart.
- `cross_session_cursor_advance_rejected` — `c>=other-sess/name` returns parse-resolution error.
- `cross_session_bookmark_read_still_works` — `b>=other-sess/name` returns records as today.
- `cursor_advanced_to_field_populated` — result types include the field; `None` when no `c>=` in filter, `Some(seq)` otherwise.
- `cursor_can_be_read_pure_via_b` — `c>=name` then `b>=name` returns the same lower bound (read doesn't advance).
- `cursor_evicted_under_churn_auto_recreates_with_full_buffer` — fill the buffer past the cursor's seq, observe WARN log on auto-recreate, observe full-buffer flood on next `c>=name`.
- `cursor_rejected_for_traces_recent` — `traces.recent` with `c>=name` returns parse-resolution error.
- `cursor_rejected_for_logs_context` — `logs.context` with `c>=name` returns parse-resolution error.
- `multiple_cursor_qualifiers_rejected` — filter `c>=foo, c>=bar` returns parse-resolution error.
- `failed_filter_does_not_create_cursor` — `c>=fresh, l>=BOGUS` errors at parse-validate; `list_bookmarks` shows no entry for `fresh`.
- `replace_true_on_active_cursor_skips_unread` — explicit `add_bookmark(name, replace=true)` snaps the cursor forward; subsequent `c>=name` returns only post-replace records.

**SDK tests (`crates/sdk/tests/cursors.rs`):**
- `Filter::builder().cursor("name").build() == "c>=name"`.
- `Filter::builder().cursor_in("sess", "name").build() == "c>=sess/name"`.
- End-to-end: typed `broker.logs_recent(LogsRecent { filter: Some(Filter::builder().cursor("c1").build()), ... })`, assert `cursor_advanced_to.is_some()` after first call returning records.

## Out of scope (deferred / YAGNI)

- **Per-record-type cursor positioning.** A cursor tracks one seq, applies uniformly to whatever query type calls it. Mixing log and span queries on the same cursor advances based on whichever query happened last. If a workload emerges that needs separate log-seq and span-seq cursor positions, revisit then.
- **Cursor-as-trigger.** Cursors don't push notifications. If a caller wants push, register a regular trigger.
- **Cursor groups / batch advance.** Per-test scope is small; explicit `list_bookmarks` + iterate suffices.
- **`c<=name` token.** "Before the cursor" isn't a streaming operation; if you want a snapshot of past, use a regular bookmark.
- **Cursor lock / immutability flag.** Within a session, anyone who can call `c>=name` controls the advance. Cross-session advance is rejected. If a "really, never advance this" mode emerges as needed, add a flag in v2.
- **Bookmark "tombstone" replay.** When a bookmark evicts (both stores rolled past), subsequent `b>=name` errors with "unknown bookmark." A pure-read consumer that wants resilience to eviction should check `list_bookmarks` first or use `c>=name` (which auto-creates).

## Implementation impact summary

| Area | Change |
|---|---|
| Protocol structs | Modify 5 (1 list result, 1 add params, 3 query results); 0 new structs |
| MCP tools | 0 new |
| SDK typed methods | 0 new |
| SDK builder methods | 1 new (`cursor`) |
| Filter parser | 1 new lexer rule (`c>=`) |
| Filter resolver | Cursor case in bookmark_resolver |
| Bookmark store | 1 new method (`cursor_read_and_advance` + `CursorCommit` type) |
| RPC handlers | Thread `cursor_advanced_to` through 3 cursor-permitted handlers; reject `c>=` in the rest |
| Persistence | Drop timestamp, add seq + created_at to PersistedBookmark |
| Schema regen | Yes, `cargo xtask gen-schema` |
| Migration code | None (clean break) |
| Docs | Skill section on cursors; README cursor paragraph |

Substantially smaller than the original "separate cursors namespace" design, while delivering the same semantics.
