# Bookmarks Design

## Motivation

When debugging with logmon, the user often wants to scope a query to "what happened during this operation" or "compare these two attempts". Today the only way to draw such a boundary is `clear_logs`, which is destructive: history is lost, and you cannot compare before/after or two sequential attempts.

Bookmarks are named timestamps the user can drop into the log/span timeline, then reference from filter expressions to scope queries to a range. Setting a bookmark before an operation makes the boundary between "before" and "after" explicit without throwing data away, and two bookmarks define a range that can be queried or compared against another range.

## User-facing model

- A bookmark is `(qualified_name, timestamp)`. The timestamp is captured at the moment `add_bookmark` is called.
- Bookmarks are global to the daemon — every session sees every bookmark.
- The qualified name is `"{session_name}/{name}"`. The session name is the existing `SessionId` (named session name, or the UUID for anonymous sessions). Because the session registry guarantees that each name belongs to at most one live session at a time, qualified names cannot collide by construction. No separate "prefix" concept is introduced.
- In tool calls and DSL expressions, a bare `name` resolves to the current session's qualified form. A `session/name` reaches into another session's bookmarks.
- A bookmark is auto-evicted when **both** the log store and the span store have evicted everything at-or-after its timestamp — i.e. it cannot outlive the data it points at. The sweep runs lazily on `add_bookmark` and `list_bookmarks`; there is no background timer.

Example workflow:

```
add_bookmark("before")
# ... run a flaky operation that produces logs/traces ...
add_bookmark("after")
get_recent_logs(filter="b>=before, b<=after, l>=warn")
get_recent_traces(filter="b>=before, b<=after, d>=100")
```

## Architecture

A new module `src/store/bookmarks.rs` holds an in-memory `BookmarkStore`:

```rust
pub struct Bookmark {
    pub qualified_name: String,         // "session/name"
    pub name: String,                   // "name"
    pub session: String,                // "session" (the SessionId stringified)
    pub timestamp: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

pub struct BookmarkStore {
    bookmarks: RwLock<HashMap<String, Bookmark>>, // keyed by qualified_name
}
```

The store is owned by the daemon alongside `LogPipeline` and `SpanStore`, and an `Arc<BookmarkStore>` is handed to `RpcHandler`.

The store is in-memory only — bookmarks are **not** persisted across daemon restarts. Persistence would be wasted work because both ring buffers start empty after a restart, so every bookmark would immediately fail its auto-eviction predicate.

## DSL extension

The filter DSL gains one new qualifier variant in `src/filter/parser.rs`:

```rust
pub enum Qualifier {
    // ... existing variants ...
    BookmarkFilter { op: BookmarkOp, name: String },
    TimestampFilter { op: BookmarkOp, ts: DateTime<Utc> }, // internal-only, post-resolution
}

pub enum BookmarkOp { Gte, Lte }
```

### Parsing

Two new tokens, recognised in `parse_token` before the generic `selector=value` rule:

- `b>=NAME` → `BookmarkFilter { Gte, name }`
- `b<=NAME` → `BookmarkFilter { Lte, name }`

`NAME` is everything after the operator until the next unquoted comma. Allowed characters: ASCII alphanumerics, `-`, `_`, `/`. The `/` is what enables `b>=other-session/before-test`. Whitespace around the name is trimmed. An empty name produces a new `FilterParseError::EmptyBookmarkName` variant.

The parser stays pure (no daemon access) — it produces `BookmarkFilter` qualifiers but does not resolve them.

### Resolution

A new compilation step runs after parsing, immediately before query execution:

```rust
pub fn resolve_bookmarks(
    filter: ParsedFilter,
    store: &BookmarkStore,
    current_session: &str,
) -> Result<ParsedFilter, BookmarkResolutionError>
```

This walks the filter and replaces each `BookmarkFilter { op, name }` with a `TimestampFilter { op, ts }`:

- A bare name (no `/`) is looked up as `"{current_session}/{name}"`.
- A qualified name (contains `/`) is looked up directly.
- If the bookmark does not exist → `BookmarkResolutionError::NotFound(name)`. The error bubbles through the RPC handler back to the MCP caller as `bookmark not found: <name>`.

`TimestampFilter` is an internal-only variant. It is never produced by the parser and never serialized — it exists purely as the post-resolution form passed to the matcher.

### Matching

`src/filter/matcher.rs` gains one new arm: `TimestampFilter { op, ts }` compares `entry.timestamp` against `ts` with the given op. The same code path serves logs and spans because both have a `timestamp: DateTime<Utc>` field.

### Selector classification

`BookmarkFilter` and `TimestampFilter` are neither log-specific nor span-specific — they apply to both kinds of entry. The existing `is_span_qualifier` and `is_log_qualifier` helpers must return `false` for both, so the "cannot mix log and span selectors" guard in `parse_filter` does not falsely trip on a bookmark qualifier combined with either kind.

### Wiring

Every RPC handler that takes a `filter` param in `src/daemon/rpc_handler.rs` is updated to: parse → resolve → execute. The affected handlers:

- `logs.recent`, `logs.export`
- `traces.recent`, `traces.get`, `traces.slow`, `traces.logs`

Each call site needs the calling session's name, which is already available as the `session_id` parameter to `RpcHandler::handle`. The resolver is invoked with `session_id.to_string()` as the current session.

### Registration guard

Registered buffer filters (`add_filter`) and triggers (`add_trigger`) **reject** any filter containing a `BookmarkFilter` qualifier. They evaluate continuously against future entries; a bookmark filter in that context would either match nothing forever or match everything past the bookmark, then mysteriously break when the bookmark is auto-evicted. The check is a one-pass walk over `ParsedFilter::Qualifiers` looking for the variant. The error message is: `"bookmarks (b>=, b<=) are not allowed in registered filters/triggers — use them only in query tools"`.

## RPC methods and MCP tools

Three new RPC methods, each fronted by one MCP tool in `src/mcp/server.rs`.

### `bookmarks.add` → `add_bookmark`

Params: `{ name: String, replace: Option<bool> }`

- Validates `name` against `[a-zA-Z0-9_-]+` (no `/`, since the user only provides the bare name; the qualifier is added automatically).
- Computes `qualified_name = "{session}/{name}"` using the calling session.
- Captures `timestamp = Utc::now()`.
- If the qualified name already exists:
  - If `replace == Some(true)`: overwrites the timestamp and `created_at`.
  - Otherwise: returns `BookmarkError::AlreadyExists(qualified_name)`.
- Returns: `{ qualified_name, timestamp, replaced: bool }`.

### `bookmarks.list` → `list_bookmarks`

Params: `{ session: Option<String> }`

- Runs the auto-eviction sweep first (see below).
- Returns all live bookmarks, optionally filtered to those whose `session` field equals the param.
- Each entry: `{ qualified_name, name, session, timestamp, age_secs }`.
- Sorted newest first by `timestamp`.

### `bookmarks.remove` → `remove_bookmark`

Params: `{ name: String }`

- If `name` contains `/`, treat as already qualified; otherwise prepend `"{current_session}/"` (same helper as the DSL resolver).
- Removes the bookmark from the store.
- If not found → `BookmarkError::NotFound(name)`.
- Any session can remove any bookmark — the global model means cleanup is not session-owned.
- Returns: `{ removed: qualified_name }`.

A small helper function `qualify(name, current_session) -> String` is shared by `bookmarks.remove` and the DSL resolver, so the bare-vs-qualified rule lives in exactly one place.

## Auto-eviction sweep

The sweep predicate for one bookmark is:

```
fn should_evict(b: &Bookmark, oldest_log_ts: Option<DateTime<Utc>>, oldest_span_ts: Option<DateTime<Utc>>) -> bool {
    let log_gone  = oldest_log_ts.map_or(true, |t| t > b.timestamp);
    let span_gone = oldest_span_ts.map_or(true, |t| t > b.timestamp);
    log_gone && span_gone
}
```

A bookmark is removed when **both** stores have evicted past its timestamp. If a store is empty, that side counts as "evicted past everything" — so a bookmark created when both stores are empty stays alive until logs/spans arrive, at which point the predicate naturally evaluates against the new oldest timestamp.

The sweep runs at:

- The start of `bookmarks.add` (cheap; keeps the store from accumulating cruft)
- The start of `bookmarks.list` (so what the user sees is current)

It does **not** run on every log/span append — that would add overhead to the hot path for negligible benefit.

`LogPipeline` and `SpanStore` each need a small `oldest_timestamp() -> Option<DateTime<Utc>>` accessor. Both stores already keep entries in a `VecDeque` ordered by arrival, so this is `entries.front().map(|e| e.timestamp)`.

## Errors

A new `BookmarkError` enum in the bookmark store module:

```rust
pub enum BookmarkError {
    InvalidName(String),
    AlreadyExists(String),
    NotFound(String),
}
```

`FilterParseError` gains `EmptyBookmarkName`. A new `BookmarkResolutionError::NotFound(String)` is produced by the resolver and converted to a string error in the RPC layer (matching the existing pattern for `FilterParseError`).

## Testing

### Unit tests

**Parser** (`filter/parser.rs` test module):
- `b>=name` and `b<=name` parse to the right qualifier.
- `b>=other/name` parses with the qualified form intact.
- `b>=` (empty name) returns `EmptyBookmarkName`.
- Charset rejection: `b>=foo bar` (space inside the name).
- Mixing with other qualifiers: `b>=mark, l>=warn` parses without tripping the log/span mix check.
- Mixing with span qualifiers: `b>=mark, d>=100` parses similarly.
- Whitespace tolerance: `b>= mark `.

**BookmarkStore** (`store/bookmarks.rs` test module):
- Add → list returns it.
- Add duplicate without `replace` → `AlreadyExists`.
- Add duplicate with `replace: true` → timestamp updated.
- Remove existing → ok.
- Remove missing → `NotFound`.
- Auto-eviction: feed mock oldest timestamps, assert correct bookmarks survive/die for all four combinations of (log past?, span past?).
- Empty-store edge case: bookmark created with no logs/spans yet, then both stores receive newer entries — bookmark should now be evictable.

**Resolver:**
- Bare name resolves against the supplied current session.
- Qualified name passes through unchanged.
- Not-found returns `BookmarkResolutionError::NotFound`.
- Resolution rewrites multiple bookmark qualifiers in one filter.
- Resolution leaves non-bookmark qualifiers untouched.

**Registration guard:**
- `add_filter` with a filter containing `b>=foo` returns the guard error.
- `add_trigger` likewise.
- `add_filter` with no bookmark qualifier still works.

### Integration tests

In `tests/`, against a running daemon (matching the existing integration test style):

1. Start daemon, connect named session "A".
2. Send a batch of logs.
3. `add_bookmark("before")`.
4. Send a second batch of logs and a trace.
5. `add_bookmark("after")`.
6. `get_recent_logs(filter="b>=before, b<=after")` — assert returned logs are exactly the second batch.
7. `get_recent_traces(filter="b>=before, b<=after")` — assert the trace appears.
8. Connect a second session "B"; call `get_recent_logs(filter="b>=A/before, b<=A/after")` — assert visible cross-session.
9. `add_bookmark("before", replace=true)` from session A — assert the timestamp moved.
10. `clear_logs`, then `list_bookmarks` — assert both bookmarks were swept (because both stores are now empty *and* the oldest-timestamp predicate trivially holds).
11. Resolution failure: `get_recent_logs(filter="b>=ghost")` returns a clear `bookmark not found: ghost` error.
12. Registration guard: `add_filter(filter="b>=before")` returns the guard error.

## Out of scope

- Persistence across daemon restart (deliberate; see Architecture).
- Per-session ownership of bookmarks (any session can remove any bookmark).
- Bookmarks usable in registered filters or triggers (deliberately blocked).
- A dedicated `compare_bookmarks` tool — composing `b>=a, b<=b` in the existing query tools is sufficient and avoids duplicating query surface.
- Renaming bookmarks (use `remove` + `add`).
- Pre-creating bookmarks at a specific historical timestamp (`add_bookmark` always uses `Utc::now()`).
