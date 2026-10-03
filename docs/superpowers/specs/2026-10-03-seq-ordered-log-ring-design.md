# The log ring holds records in seq order

**Status:** rev 2, for approval · T2 · branch `perf/traces-logs-index` (nothing pushed).
Rev 2 folds in the fresh-context verification pass (implementer + correctness lenses); §9 lists
what it changed.

## 1. Problem

A record's **seq** is the archive's only ordering ("timestamps can tie", `cases/document.rs`).
The log ring (`store/memory.rs`, a `VecDeque`) is in the order records were **stored**, and the
two differ: when a trigger fires, `process_entry_for_domain` stores the record that fired it
first, then the older records of its pre-window that had not been stored yet
(`daemon/log_processor.rs:90`, then `:98` and `:108`).

Every reader that treats a position in the ring as a position in seq is therefore wrong across
a trigger's stored stretch. Gate rounds on this branch found them one at a time:

| Reader | Wrong how | Status |
|---|---|---|
| `resolve_case_anchor`, trace-id arm | took the first stored record as "earliest by seq" | fixed (5c360bb) |
| `create_case` window (`context_by_seq`) | farther records took nearer ones' slots; logdata written out of seq order, which `cases::load` refuses; a false "gone" claim | fixed by a seq scan (b354baf), which costs two S2s of its own |
| `lost_below` (`memory.rs` eviction, `clear`) | `evicted.seq + 1` assumes eviction in seq order: it can sit above held records, and go DOWN | open |
| trigger notification `context_before` (`log_processor.rs:122`) | positional slice that misses the records just flushed | open |
| `logs.context` (`rpc_handler.rs:1516`) | positional slice, returned in stored order | open |
| bookmark anchor (`rpc_handler.rs:2786`) | first match in ring order, not the lowest seq after the mark | open |
| count-limited cursor reads (`logs.recent`/`logs.export` with `c>=`) | walk oldest-first in ring order and commit the highest seq returned, so held records below it are skipped for good — over `[91, 100, 95, 96, 97]`, `c>=90 count:2` returns `[91, 100]` and never 95-97 | open |
| "newest first" readers, `buffer_oldest_seq`/`buffer_newest_seq`, `domains.list` oldest/newest | a late-flushed pre-window record reads as the newest; `front()`/`back()` are not the min/max | open |

Patching each reader is whack-a-mole, and the next reader written will assume seq order again
— reasonably, since seq is the documented ordering.

**Confirmed (duty 0, and again by the verification pass):** seqs are assigned only at
`log_processor.rs:53` (logs) and `span/store.rs:105` (spans). In production exactly two append
sites write out of seq order, both in the trigger branch (`log_processor.rs:98`, `:108`); `:90`,
`:173` and `:196` each store the record whose seq was assigned at the top of that same call, and
one processor task per domain (`domain_lifecycle.rs:102`, `server.rs:393`) makes that the newest.
Logs are never persisted. `from_records` is reached only from `load_case`, after `validate_seqs`.

## 2. Decision

**The ring's order is seq order — an invariant the STORE establishes, not one its callers
keep.** Every reader may rely on it.

## 3. Mechanism

### 3.1 The store's two writers

**`append(entry)`** (the `LogStore` trait method; its contract changes and is documented):
1. **Floor first:** `entry.seq < lost_below` → refused (see §3.3). Checked before the fast path,
   because after `clear` the ring is empty while the floor is not.
2. **Fast path, the normal case:** ring empty or `entry.seq > back().seq` → evict-if-full, then
   `push_back`, exactly as today.
3. **Already held:** a seq equal to one held is skipped (callers check today; the store must not
   depend on it).
4. **Otherwise** → `insert_sorted(vec![entry])`.

**`insert_sorted(batch)`** — inherent on `InMemoryStore` (precedent: `for_each_matching`), with a
`LogPipeline` wrapper:
1. **Owns its input:** sorts the batch by seq and drops duplicates within it and seqs already
   held — the trigger's natural batch is NOT sorted (the pre-window drain returns the newest
   entries, then the trace read returns OLDER ones still in the pre-buffer), and a merge fed an
   unsorted batch would corrupt the ring with every binary search then answering wrongly and
   silently.
2. **Floor:** drops seqs `< lost_below`.
3. **Merge:** `split = partition_point(seq < batch[0].seq)`; drain `entries[split..]` (the tail)
   into a scratch vec; merge tail and batch by seq.
4. **Capacity, BEFORE pushing back:** if `split + merged.len() > max_capacity`, the lowest
   `excess` of the ring — the front, and then the lowest of the merged sequence if the front runs
   out — leave as evictions: each one `fetch_max`es the floor to its seq + 1 and is unindexed
   from `trace_index`. Pushing back past capacity would reallocate the deque beyond its one-time
   `reserve_exact`, and `pop_front` never shrinks it.
5. **Push back** the merged sequence; run the lazy `reserve_exact` on this path too.
6. **`trace_index`:** for each trace in the batch, merge that trace's (already ascending) list with
   the batch's seqs for it once — not a `partition_point` insert per record, which is the same
   O(batch × tail) shape this design rejects for the ring.
7. Returns the number of tail records moved (exposed for the cost test and as a metric).

**The trigger branch** (`log_processor.rs` ~85-110), per firing session, as now: copy the
pre-window (the drain) FIRST, then read the trace's entries (in that order — the drain removes
what the trace read would otherwise duplicate), collect both, and call `insert_sorted` once,
before that session's `context_before` is built (`:122`). The per-candidate `contains_seq` checks
go (the store skips held seqs). The trigger's own record (`:90`) is the newest seq and takes
`append`'s fast path.

### 3.2 Finding a record

`seq_pos` and `popped` (473b371) go. With the ring ascending, a seq is found by
`binary_search_by_key`; no second structure to keep in step, and no position that shifts under a
merge. `trace_index` lists stay ascending, so `logs_by_trace_id` is O(k log n) over the trace's own
seqs. `context_by_seq` locates by binary search instead of a linear `position()`.
`contains_seq` answers the common case — the seq just assigned, `log_processor.rs:168` — by
comparing with `back().seq` in O(1), and binary-searches only otherwise.

### 3.3 Records the store refuses, and the counters

- **Below the floor** (`seq < lost_below`): older than something already evicted, or than a
  `clear`. Refused. **Behaviour change, stated:** after `logs.clear` the floor is the newest seq
  held at clear time plus one, so a trigger firing later can no longer bring back records from
  before the clear (today it re-appends them, cleared records included). On a small, densely
  stored ring, today's code re-appends records that had been evicted, pushing out newer ones —
  even the trigger's own record; that stops too.
- **Counters:** `total_received` counts every record offered to the store; `total_stored` every
  record inserted. A record refused below the floor or skipped as already held counts as
  received only, and an excess dropped during a merge (§3.1 step 4) counts as stored and then
  evicted, as any eviction does. `total_received ≠ total_stored` becomes possible for the first
  time; both are on the wire (`status.get`, `rpc_handler.rs:1755`), so the CHANGELOG says so.

### 3.4 Readers this makes correct with no change of their own

`context_by_seq` and so the case window and `logs.context`; the notification's `context_before`;
the bookmark anchor; `lost_below` and `clear`; every newest-first reader; `buffer_oldest_seq` /
`buffer_newest_seq` and `domains.list`'s oldest/newest; `traces.logs`, `traces.get` and
`logs.recent{trace_id}` (now ascending by seq); `logs.fields`' name cap (now judged in seq order);
and count-limited cursor reads, which stop skipping held records (§5).

### 3.5 What this branch then undoes or keeps

- **Reverted:** b354baf's window rewrite. The positional `context_by_seq` slice it replaced is now
  exact (every log among the merged nearest `before` is among the nearest `before` logs, so the
  slice contains it, and `short_before` comes out identical), and it is one read under one lock
  with no ring-sized allocation. Its logdata and spandata sorts become no-ops and go.
- **Kept:** the anchor's `min_by_key(seq)`; `trace_spans` shared by `get_trace` and
  `recent_traces`.
- **Rewritten:** 473b371's position map → binary search (§3.2).

### 3.6 The span store

- `SpanStore::insert` assigns the seq UNDER its write lock (today it takes it before), so the span
  ring is seq-ordered by construction. The returned seq is unused (`span_processor.rs:47`).
- Its `seq_pos`/`popped` become a binary search; its `context_by_seq` locates by binary search;
  its `lost_below` uses `fetch_max`.
- `SpanStore::from_records` (and `InMemoryStore::from_records`) **assert** ascending, distinct
  input, rather than trusting the caller; `load_case` already validates, so production is
  unaffected, and the span test that feeds `[105, 103, 109, 104]` is rewritten.
- The case window's span seq scan (`rpc_handler.rs:2379`) could then be a `partition_point` slice;
  left as is here (it was already the shape before this branch).

## 4. In the same change: a window's eviction is mis-graded (pre-existing, both stores)

The window's lower end merges log and span seqs.

- **Log side (S1):** `from` can be a span below the log ring's floor. Logs in `[from, lost_below)`
  were stored and evicted, yet `short_before == 0` because spans filled `before`, so `log_evicted`
  is false and the document says nothing was evicted.
- **Span side (mirror):** `short_before > 0` with the span ring evicted and the log ring not:
  `log_evicted` is false, `span_evicted` is `None` because the span floor is at or below `from`,
  and the document says the span ring "had dropped nothing below seq {from}" and calls the
  shortfall "an empty past", though dropped spans would have filled the window.

**Fix:** a `Window` field `logs_evicted_before_window: Option<u64> =
evicted_below(from, log_lost_below)` (`filter/parser.rs:847`; `from` inclusive), set beside the
span one, feeding `log_evicted`; and for the span mirror, the span shortfall judged against the
span floor whether or not the span floor is above `from`. The document renders the log-side
line whatever the verdict (as the span line does — `Filtered` outranks `Evicted`,
`engine/epoch.rs:277`), and these four spots stop assuming the old cases: the Evicted verdict
paragraph and its note (`document.rs:546-563`), "Below the window" (`:598-614`), the next-step
item (`:860-867`), and the span line (`:686-693`). Every comment asserting "the window's lower
end IS the store's floor" is corrected (`rpc_handler.rs` window, `document.rs:102-111`,
`parser.rs:863`).

## 5. Cursors

Seq order fixes count-limited cursor reads over records already held (§1's table). What remains
is LATE ARRIVAL: a record stored after a cursor has passed its seq — a trigger's pre-window,
flushed later — is still skipped, since the cursor commits the highest seq it returned. That is
a semantic decision (track a per-cursor boundary in stored order, or document that pre-trigger
context reaches only non-cursor reads), scoped as the drafted logmon issue, out of this change.

## 6. Tests (two flavors)

- **Invariant against an independent model (verification):** a seeded run of appends in order,
  unsorted batches with internal duplicates (the trigger shape), below-floor arrivals, merges
  that overflow capacity, evictions and clears. The oracle is a MODEL kept beside the store — the
  set of records that should be held (offered at or above the floor, minus capacity overflow and
  clears) and the exact `lost_below` — not a scan of the ring, which a merge that silently drops
  records would still pass. Checked after every step: the ring equals the model, ascending;
  `logs_by_trace_id`, `contains_seq`, `context_by_seq` equal their definitions over the model;
  `lost_below` equals the model's.
- **End to end through a real trigger (bug bounty):** an "errors only" filter in another session
  plus a trigger with a pre-window; after it fires: `logs.recent`'s newest is the trigger's own
  record; `logs.context` around it includes the pre-window in seq order; a bookmark anchor takes the
  lowest seq after the mark; a count-limited cursor read returns the held records in seq order
  without skipping; a case captured over the stretch loads and holds the `before` records
  nearest by seq; `lost_below` stays monotonic across an eviction of the stretch.
- **§4:** a capture whose window spans below the log floor reports eviction; the span mirror.
- **Cost:** `insert_sorted` returns the records it moved; assert the bound of §8's chosen option.
- **Existing tests that change:** `memory.rs:757-767` (expects `[10, 5]`, becomes `[5, 10]`); the
  `assert_positions` helpers (no `seq_pos`); the late-append differential (late appends can now be
  refused below the floor — its oracle models that); `memory.rs:411-454` (appends many
  `LogEntry::synthetic`, all `seq: 0` — given real seqs); the span `from_records` test;
  `tests/cases_rpc.rs` (its vacuity guard asserted `traces.logs[0]` is the trigger's record,
  which seq order makes impossible; it asserts instead that the trace's INFO records are held at
  all — the "errors only" filter keeps them out, so only the trigger's flush can have stored them).
- Every new assertion negative-controlled.

## 7. Cost, risk, and what users see

- Ingestion's normal path: one more comparison. A trigger fire: one tail merge under the write
  lock, bounded per §8.
- **User-visible:** a late-flushed pre-window record is no longer the newest in `logs.recent`;
  `traces.logs`/`traces.get`/`logs.recent{trace_id}` are ascending by seq; buffer oldest/newest
  are the true min/max; count-limited cursors stop skipping; `total_received` can exceed
  `total_stored`; after `logs.clear`, a trigger no longer brings back pre-clear records. CHANGELOG.
- **Risk:** the ring's order becomes load-bearing for every reader, so a writer that bypassed it
  would silently break them all. Guard: the store establishes the order itself — `append` routes
  anything not on the fast path through the merge, `from_records` asserts its input — so no caller
  can store out of order; the model test drives every writer the store exposes.
- **Stale prose to correct** with the code: `rpc_handler.rs:2226-2229`, `memory.rs:321`,
  `document.rs:133`, `load.rs:325-329`, `domain.rs:145-150`, and this branch's three CHANGELOG
  entries that say "the order records were stored" (and the one that calls 500,000 records the
  default buffer — the default is 10,000).

## 8. Decision for the owner: the merge's worst case

A merge moves every held record whose seq is above the batch's lowest. The first draft claimed
that is bounded by the pre-buffer's size; **the verification pass refuted it.** A flush DRAINS the
pre-buffer's newest end and leaves its older entries, so the buffer's front can sit far older
than its capacity, and the trace read reaches back to that front. With two triggers of different
`pre_window` (say 5000 and 19, the small one firing every ~20 records), the large one's batch can
reach ~100k records back, and its merge moves the whole ring: 10k records on the default buffer,
500k on a configured one — tens of ms under the write lock, on the ingest path. With one shared
`pre_window` (the defaults), each flush empties the buffer and the bound holds.

- **Option A (recommended): make the pre-buffer exactly "the last N arrivals".** Stamp each entry
  with its arrival index and expire entries older than the last `capacity` arrivals, drained or
  not. Every batch then lies within the last N arrivals, so a merge moves at most N records (the
  domain's largest `pre_window`). Changes behaviour only in the mixed-pre-window case: a trigger no
  longer pulls in same-trace records older than its buffer's N arrivals, which it only ever
  reached because other flushes had punched holes in the buffer.
- **Option B: accept the worst case**, documented with its numbers, plus the moved-records metric
  so it is visible when it happens.

## 9. What rev 2 changed (from the verification pass)

The batch is sorted and deduped by the store (it is not sorted as produced); capacity is
enforced before pushing back; trace lists merge once per trace; the floor is checked before the
fast path; counters are defined; `contains_seq` has an O(1) common case; the span store asserts
its input and uses `fetch_max`; the §4 fix names its four rendering spots and adds the span-side
mirror; cursors are credited, not just deferred; the tail bound was refuted and became §8; the
model-based oracle replaces a self-referential one; the tests that change are listed.
