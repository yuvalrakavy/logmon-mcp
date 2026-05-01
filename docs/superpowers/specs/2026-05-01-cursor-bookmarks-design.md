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

#### `seq = 0` is reserved as a sentinel

`SeqCounter` starts assigning at `seq = 1`, never `0`. This makes `seq = 0` a safe sentinel for "the cursor has never advanced," used by auto-create (`c>=name` on missing entry → entry stored with `seq = 0` → filter `entry.seq > 0` matches every record ever ingested). Without the reservation, the very-first-record case would silently skip the first record on the first auto-create read.

Implementation: `SeqCounter::new` and `SeqCounter::new_with_initial(initial)` both ensure the first allocation returns `max(initial, 1) + 1` (or equivalent — the contract is "the value `0` is never returned by `next_seq()`"). Existing tests that assert on specific seq values may need to shift by 1.

#### Bookmark name charset

Cursor names share the existing bookmark name charset, validated by the existing `is_valid_bookmark_token` function in `crates/core/src/filter/parser.rs`: `[a-zA-Z0-9_-/]+`. The `/` is reserved as the session/name separator (e.g. `c>=other-sess/cursor-name`); a name containing `/` is interpreted as cross-session-qualified. A cursor name with multiple slashes (`a/b/c`) is rejected at parse time, same as today's bookmark behavior.

### DSL

| Token | Operation |
|---|---|
| `b>=name` | Filter records to `entry.seq > bookmark.seq`. Pure read. Errors if `name` is unknown. |
| `c>=name` | Same filter, then atomically update `bookmark.seq = max(returned.seq)` after the query completes. Auto-creates the bookmark at `seq = 0` if `name` is unknown. |
| `b<=name` | Filter records to `entry.seq < bookmark.seq`. Pure read. Errors if `name` is unknown. (Existing token, semantics unchanged modulo position type.) |

Both `b>=` and `c>=` accept cross-session reach via `other-session/name`:

- `b>=other/name` — allowed, read-only.
- `c>=other/name` — **rejected at parse-resolve time** with `"cross-session cursor advance is not permitted"`. Mutating another session's bookmark from outside is an attractive footgun, never useful in practice.

Both tokens are rejected by `add_filter` and `add_trigger`. Today the existing `b>=`/`b<=` rejection is keyed on a generic predicate `contains_bookmark_qualifier(parsed_filter) -> bool` in `crates/core/src/filter/parser.rs` (called from `crates/core/src/daemon/rpc_handler.rs::handle_filters_add` / `handle_triggers_add`). Extend this predicate to also match the new `c>=` qualifier variant, and update the user-facing error string from `"bookmarks (b>=, b<=) are not allowed in registered filters/triggers"` to `"bookmarks and cursors (b>=, b<=, c>=) are not allowed in registered filters/triggers"`. One predicate, one error site, no duplication.

The token's behavior is determined entirely by which token is used. The same bookmark can be referenced via either token by the same session — the bookmark itself has no "this is a cursor" flag.

#### Where `c>=` is permitted

`c>=` requires the query result to consist of seq-anchored records so the advance value is well-defined. Allowed query methods:

- `logs.recent`
- `logs.export`
- `traces.logs`

**Result ordering when `c>=` is present.** Today these methods return records newest-first (typical "recent" semantics). When the filter contains a `c>=` qualifier, the result is reordered to **oldest-first within the cursor's window**, so that cursor advance + paginated re-reads drain the buffer monotonically. Concretely: with cursor at `seq = 10` and `count = 100`, the call returns records with seqs in `[11, 110]` (oldest 100 matching the rest of the filter), and advances to `110`. Next call returns `[111, 210]`. Without the reordering, a buffer of 10 000 records polled with `count = 100` would silently drop 9 900 — the caller would only see the most-recent 100 and skip past everything older. The ordering shift applies only when `c>=` is present in the filter; non-cursor calls remain newest-first.

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
| `add_bookmark` | Existing `description: Option<String>` param preserved unchanged. New: optional `start_seq: u64` (default = current seq counter). New: optional `replace: bool` (default false). On existing name with `replace=false`, errors with `"bookmark exists; pass replace=true to overwrite"`. With `replace=true`, overwrites unconditionally — note that this is destructive against an actively-polled cursor: the cursor snaps to `start_seq` and any unread records below the new position are skipped on the next `c>=` read. |
| `list_bookmarks` | Result entries return `name`, `seq: u64`, `created_at: <ISO 8601 string>`, `description: Option<String>`. The legacy `timestamp` field is removed. |
| `remove_bookmark` | Unchanged. |
| `clear_bookmarks` | Unchanged. |

`add_bookmark` without `start_seq` and `replace` is wire-compatible with today's call sites (the new params are optional with serde defaults).

### Result-shape change

Three result types — exactly the methods that accept `c>=` per the allow-list above — gain `cursor_advanced_to: Option<u64>`:

- `LogsRecentResult`
- `LogsExportResult`
- `TracesLogsResult`

Populated as `Some(new_seq)` when the query's filter included a `c>=...` qualifier AND at least one record matched (the cursor actually advanced). `None` in two cases: (a) filter contained no `c>=...` qualifier; (b) `c>=...` was present but no records matched, so the cursor is unchanged. Callers that need to know "what's my cursor seq right now regardless" should call `list_bookmarks`.

For `logs.export`: today's handler returns the matched records inline in the result (`{ logs: [...], count: N, format: "json" }`). Cursor advance accumulates `max(returned.seq)` over the same materialized vector — no streaming-write semantics required. If `logs.export` ever evolves to stream to a file, the cursor commit must move to after the successful flush so a write failure aborts the advance.

### SDK changes

**Typed methods:** `BookmarksAdd` gains `start_seq: Option<u64>` and `replace: bool`. `BookmarkInfo` (the `list_bookmarks` element type) gains `seq: u64` and `created_at: DateTime<Utc>`, drops `timestamp`. The three query result types listed above (`LogsRecentResult`, `LogsExportResult`, `TracesLogsResult`) gain `cursor_advanced_to: Option<u64>`. No new method.

**Filter builder:** one new method.

```rust
impl FilterBuilder {
    /// Read-and-advance: emits "c>=<name>". Auto-creates the bookmark at
    /// seq=0 server-side on first reference. Cross-session cursor advance
    /// is intentionally not buildable — the broker rejects it. To read
    /// another session's cursor position pure-read, use `bookmark_after`
    /// with a `session/name` argument.
    pub fn cursor(mut self, name: &str) -> Self {
        self.qualifiers.push(format!("c>={name}"));
        self
    }
}
```

No `cursor_in(session, name)` method — exposing a builder that always emits a server-rejected form is a footgun. Cross-session cursor advance is intentionally unsupported; cross-session pure-read uses `bookmark_after("session/name")` instead.

The existing `bookmark_after(name)` and `bookmark_before(name)` are unchanged.

### Persistence and eviction

**Persistence.** Named-session state.json keeps its existing `bookmarks: Vec<PersistedBookmark>` shape, with `PersistedBookmark { name, seq, created_at, description }`. Anonymous-session bookmark entries are dropped from the in-memory store when the session disconnects (the entire `SessionState` is removed today; this includes any bookmark/cursor entries scoped to that session).

**Loading an old `state.json`** (from a pre-cursor broker version): the new struct's required fields (`seq`, `created_at`) won't match the old shape (which has `timestamp` instead of `seq`). On deserialize failure for the `bookmarks` vector specifically, the broker logs at WARN (`"discarding bookmarks from previous-version state.json: <serde error>"`) and starts with an empty bookmarks store. The rest of state.json (`seq_block`, `named_sessions` structure, triggers, filters, client_info) loads normally. The user re-creates any bookmarks they need; daemon doesn't refuse to start.

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

The query path uses two brief lock acquisitions bracketing a lock-free query:

1. Parse + validate the filter (no lock held).
2. **First lock acquisition.** Call `cursor_read_and_advance(session, name)` → atomically gets-or-creates the entry, returns `(lower_bound, commit_handle)`. Lock released.
3. Execute the store query with `entry.seq > lower_bound` plus the rest of the filter. **No lock held during this phase** — other sessions' bookmark operations proceed in parallel.
4. Compute `cursor_advanced_to = entries.iter().map(|e| e.seq).max()`.
5. **Second lock acquisition.** Call `commit_handle.commit(cursor_advanced_to)` — re-acquires the lock briefly to update the entry's seq. No-op if no records returned (i.e. `cursor_advanced_to` would be `None`).
6. Return result with `cursor_advanced_to` populated.

**Eviction race during the lock-free query phase.** If an eviction sweep removes the bookmark entry between steps 2 and 5 (because its seq fell below both stores' oldest seqs while the query was running on historical buffer content), `commit_handle.commit(seq)` finds the entry gone. The commit re-inserts the entry at `max_returned_seq` with `created_at = now()`, preserving advance intent — the cursor effectively "rebuilds itself at the new high-water mark." A subsequent `c>=name` from the same session continues from there, not from `seq = 0`. This avoids the surprising "I just queried successfully but my cursor is now reset" failure mode.

(The eviction sweep itself is the only writer that removes entries other than explicit `remove_bookmark` / `clear_bookmarks` / session disconnect; commit re-insert is safe under the same lock.)

### Migration

**State.json:** None. The new broker version treats an old-shape `bookmarks` vector as a deserialize failure → WARN log + empty bookmarks store, daemon starts normally (per §Persistence above).

**Wire / SDK:** This release breaks `BookmarkInfo` (drops `timestamp`, adds `seq` + `created_at`) and `BookmarksAdd` (adds optional `start_seq` + `replace`). At v1, the only SDK consumer outside this repo's tree is the in-development store-test integration (Spec B); the MCP shim and broker-sdk crate ship together with the broker, so the break is contained. Future protocol versions must use additive-field discipline (no removed fields) — the `BookmarkInfo` field removal is a one-time cleanup tied to the cursor-mechanism introduction at v1, not a precedent.

CHANGELOG note: bookmark filtering (`b>=`, `b<=`) is now seq-based rather than timestamp-based. For live ingestion this is invisible because the broker assigns seq within microseconds of receipt; divergence would only manifest under bulk-replay (loading historical logs whose `timestamp` field predates their assignment-time) or clock-skew (a GELF client whose host clock drifts relative to the broker's). Neither is a documented workload for this broker.

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
- `cursor_paginates_oldest_first_within_window` — buffer with 250 matching records; cursor at 0; `c>=cur` with `count=100` returns the oldest 100, advances; second call returns next 100; third call returns last 50; cursor advances each time.
- `cursor_eviction_during_query_re_inserts_at_high_water` — start a long-running query, evict the cursor entry mid-flight, verify commit re-inserts the entry at `max(returned.seq)` rather than dropping the advance.
- `b_le_uses_seq_position` — `add_bookmark("anchor")`, ingest 5 records, `b<=anchor` returns records with `seq < anchor.seq` (i.e. nothing — anchor was created at current seq before the 5 records arrived). Confirms `b<=` semantics shifted from timestamp to seq cleanly.
- `seq_zero_never_assigned` — sanity test that the broker's first record gets `seq >= 1`, not `0`. Direct test of the SeqCounter contract.
- `anonymous_session_disconnect_drops_bookmarks` — anon session adds bookmark, disconnects, reconnects under a new anon ID, `list_bookmarks` is empty.

**SDK tests (`crates/sdk/tests/cursors.rs`):**
- `Filter::builder().cursor("name").build() == "c>=name"`.
- `Filter::builder().bookmark_after("sess/name").build() == "b>=sess/name"` — confirms cross-session pure-read still composable through the existing builder.
- End-to-end: typed `broker.logs_recent(LogsRecent { filter: Some(Filter::builder().cursor("c1").build()), ... })`, ingest known seqs, assert `result.cursor_advanced_to == Some(entries.last().seq)` (exact value, not just `is_some()` — catches a bug where the field is always populated with `Some(0)`).

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
| Migration code | State.json: tolerate-and-discard old bookmark shape (one Result match in load_state); SDK: none |
| Docs | Skill section on cursors; README cursor paragraph; update existing bookmark guide where it asserts timestamp semantics for `b>=`/`b<=` |
| SeqCounter | Reserve seq=0 as sentinel; first allocation returns ≥1 |

Substantially smaller than the original "separate cursors namespace" design, while delivering the same semantics.
