# A cursor delivers records stored late (gh #23)

Status: design, T2 (changes the cursor contract on three wire methods).

## 1. Problem

A cursor (`c>=name`, accepted by `logs.recent`, `logs.export`, `traces.logs`) is a single seq. A read
returns matching records with `seq > position` and commits `position = max seq returned`
(`rpc_handler.rs:1489-1499`, `bookmarks.rs:50-74`).

A trigger stores records LATE: when it fires, it merges its pre-window and its trace's buffered
entries into the ring (`log_processor.rs:102-109`, `InMemoryStore::insert_sorted`). Those records
carry seqs lower than records already stored. A cursor that read before the flush has passed their
seqs and never returns them.

Reach: only when some session in the domain has or had a filter (with no filters every record is
stored on arrival; records a since-removed filter kept out sit in the pre-trigger buffer and can still
be flushed late). The Store test harness drains each lane through `traces.logs` with
`c>=st-<trace>` and asserts no WARN reached a script's bucket; a filter added by ANY session in that
lane's domain makes an ERROR trigger's pre-window land behind the harness cursor, so WARNs in it are
never seen — a false green.

## 2. Contract (new)

> A cursor considers every record stored in its domain exactly once: the first cursor read that can
> see it returns it if it matches that read's filter, and passes it by for good if not (as a cursor
> always has). A record stored after the cursor already passed its seq — a trigger's late flush — is
> considered by the next read. The reply counts those it returns in `cursor_late`, and the late
> records that left the buffer before any read could consider them in `cursor_late_lost`. Both are
> absent when zero (the renderer prints every present key), and absent on non-cursor reads.

Records in a reply stay in ascending seq order (the late ones therefore come first). Consequence a
reader must accept: within one cursor's stream, seqs are no longer monotonic across replies.
`cursor_advanced_to` keeps its meaning (the cursor's seq position after the read) and is still
absent when the position did not move — which now includes a reply of late records only.

Unchanged: `b>=`/`b<=` (pure reads, no delivery claim); every non-cursor read; eviction of a record
above the cursor (still graded by `evicted_before_window` over the `seq > position` window). The
`logs.export` verdict window is still `(position, to]`: it does not vouch for late records, which
sit below it.

## 3. Mechanism

### 3.1 Which records are late

A record is late iff it is stored with a seq below the newest seq held at that moment. Only the merge
path (`merge_locked`, reached from `insert_sorted` and from `append`'s slow path) can store one; the
`append` fast path stores a seq above everything held by construction.

Sufficiency rests on one property of the writer, not of the cursor: **per domain, every store that
is not a merge happens in the same processor call that assigned the record's seq**
(`process_entry_for_domain` assigns, then stores on arrival — trigger record, post-window, filter —
and one task runs per domain). So stores outside the merge are in seq order, and every out-of-order
store goes through the merge, below a held seq. In particular a trigger stores its OWN record first
(`log_processor.rs:87-92`) and only then its batch (`:109`), so every flushed record is below a held
seq and is recorded late.

Consequence used in §3.4: after a cursor read that returned records, the cursor's position `L'` is a
seq that was held, so any record stored afterwards with `seq <= L'` is late. (A record's seq is never
stored twice, so `seq == L'` cannot recur.)

### 3.2 Store state

`StoreInner` gains:

- `late_counter: u64` — incremented once per late record stored. Never persisted; a new store starts
  at 0.
- `late: BTreeMap<u64 /*late number*/, u64 /*seq*/>` and `late_by_seq: BTreeMap<u64 /*seq*/, u64>` —
  the late records still held (`late_by_seq` ordered so its minimum seq is cheap: a read whose
  position is below every late seq skips the late part outright). A record leaves the ring three
  ways, and each forgets it: `evict_front` and the merge's overflow drop both already call
  `StoreInner::unindex` (`memory.rs:269`, `:342`), which becomes the single "a record left" hook for
  the trace index AND the late maps — the late removal BEFORE `unindex`'s early return for an
  untraced record; `clear_locked` clears both maps. The merge numbers only the records it KEEPS (after its overflow drop — a ring of
  capacity 0 keeps none), in ascending seq order within the batch. Both maps are therefore bounded by
  the ring capacity and empty on a domain where nothing was ever stored late.

No `LogEntry` field: on-arrival records never need a late number (§3.4), and `LogEntry` has 49
struct-literal construction sites.

### 3.3 Cursor state

`Bookmark` gains `late_mark: u64` (meaningful only when used as a cursor): the store's
`late_counter` observed by the cursor's last read. Set on every creation path:

- auto-create by a `c>=` read: `(seq 0, late_mark 0)` — "everything held";
- `bookmarks.add` (and `replace`): `(seq counter, late_mark = the store's late_counter now)` — "from
  now", so late records stored BEFORE the add do not replay into a cursor that means "only what
  arrives after this call" (`skill/logmon.md`, the SDK README). The add needs the domain store's
  counter passed in; `BookmarkStore` cannot see the store;
- restore after a restart: `late_mark 0` — `PersistedBookmark` does not carry it, which is correct
  because a restarted daemon's store is empty and its `late_counter` restarts at 0.

The read obtains `(L, M)` from the commit handle (`CursorCommit` gains `late_mark()`).

`c>=` resolves to a dedicated `Qualifier::CursorSeq { after: L }` instead of `SeqFilter { Gt, L }`.
It matches exactly as `seq > L` everywhere a filter is evaluated or a window is derived
(`evicted_before_window`, `resolved_seq_range`, the matchers, diff keys). It is distinct only so the
cursor read can tell the cursor's own bound apart from an explicit `from_seq`/`to_seq` range
(`lower_seq_range`, which also emits `SeqFilter`): a late record below an explicit `from_seq` must
still be excluded.

### 3.4 A cursor read

Under ONE store read lock, with cursor `(L, M)`, filter `F` (containing `CursorSeq{after: L}`), limit
`N` (`usize::MAX` for `traces.logs`, which takes no count):

1. **Late part.** Skipped outright if `L` is below the minimum seq in `late_by_seq`. Otherwise walk
   `late` for numbers `> M`, ascending by late number. For each, find the record by seq (binary
   search; the ring is seq-ordered); take it iff `seq <= L` and it matches `F` with `CursorSeq`
   treated as true. Stop at `N`. `traces.logs` (no count, so order is free) instead walks the
   TRACE's own seqs `<= L` from `trace_index` and keeps those whose `late_by_seq` number is `> M`:
   O(trace), never the domain-wide late map — `traces.logs` was made O(k log n) on purpose
   (`memory.rs:550-557`) and the Store harness polls it per trace.
2. **Normal part.** If fewer than `N` were taken, walk the ring oldest-first exactly as today (`F`
   includes `seq > L`), taking up to the remaining budget.
3. **Snapshot.** Read `late_counter` as `S`.

The store returns the records in this DELIVERY order, each tagged with its late number (late part)
or none (normal part), plus `late_exhausted` (the late walk ran out of candidates rather than out of
budget) and `S`. The handler may keep only a PREFIX of that list — `logs.export` asks for `count + 1`
to learn `capped` and keeps `count` — so the commit is a pure function of the KEPT prefix:

- If any late candidate is not in the kept prefix (`!late_exhausted`, or the prefix ends inside the
  late part): `L' = L`, `M' =` the late number of the last kept late record (`M` if none kept).
- Otherwise (every late candidate was kept): `M' = S`; `L' = max(L, max seq kept from the normal
  part)`, unchanged if none.

`cursor_late` = the number of kept records from the late part. The reply lists the kept records in
ascending seq order (late ones have `seq <= L`, so they come first).

`cursor_late_lost` = `(M' − M) − |late entries numbered in (M, M']|`, read under the same lock:
numbering is dense and every number belongs to a record the merge kept, so a number in that range
no longer in the map is a late record that left the ring (evicted, or cleared) before this read
could consider it.

The commit happens whenever `(L', M') != (L, M)` — including a reply with no records (the late mark
still moves) and a reply of late records only (the seq position does not). `CursorCommit::commit`'s
guard (`max_returned_seq <= lower_bound` ⇒ no-op) and the handlers' "commit only if a record came
back" both go.

Exactly once, case by case:

- *Late part cut.* Taken late records have numbers `<= M'`; untaken candidates have numbers `> M'`
  (ascending walk) and are found next read. `L` unchanged, so no normal record is skipped.
- *Late part complete.* Every candidate numbered in `(M, S]` with `seq <= L` was taken. A record
  numbered `<= S` with `seq > L` belongs to the normal part: taken iff `seq <= L'` (the normal walk is
  seq-ordered), otherwise `seq > L'` keeps it a candidate. A record stored after the read either has
  `seq > ` every seq held at the snapshot (on-arrival, so `> L'`) or a late number `> S = M'`.
- *No duplicates.* A record taken in the late part has `seq <= L <= L'` and number `<= M'`, so it is
  neither a normal nor a late candidate afterwards. A record taken in the normal part has
  `seq <= L'`, and its late number (if any) is `<= S = M'`.

The whole read (both parts and `S`) is under one lock, so a concurrent flush lands either entirely
before (seen now) or after (numbered `> S`).

### 3.5 Where it is wired

- `InMemoryStore::cursor_read(count, filter, (L, M)) -> CursorRead` and `cursor_read_trace(trace_id,
  filter, (L, M)) -> CursorRead`, where `CursorRead` holds the records in DELIVERY order each tagged
  with its late number (or none), `late_exhausted`, `S`, the ring view, and a pure
  `advance_for(kept: usize) -> CursorAdvance { seq, late_mark, late, late_lost }`. The STORE never
  decides the commit; the handler calls `advance_for` with the prefix it keeps. `LogPipeline`
  forwards both.
- `BookmarkStore::cursor_read_and_advance` returns the handle; `CursorCommit` exposes
  `lower_bound()`/`late_mark()` and `commit(seq, late_mark)` (the re-insert-after-sweep path keeps
  both). `bookmarks.add` takes the late counter.
- `logs.recent`, `logs.export`, `traces.logs`: call the cursor read when a commit handle is present;
  `logs.export`'s `capped` probe (`count + 1`) applies to the combined budget and the commit uses the
  kept prefix. Add `cursor_late` / `cursor_late_lost` (each absent when 0).
- `Qualifier::CursorSeq` sites: the matchers, `admission.rs` (exhaustive), `diff.rs` key, and the two
  SILENT ones — `resolved_lower_bound` (`parser.rs:779-797`, `_ => None`; missing it drops
  `truncated` for every cursor read) and `resolved_seq_range` (`parser.rs:811-830`).
- Protocol: both fields on the three result types (`cursor_late_lost` not on `traces.logs`) + the
  generated schema. The renderer is structural (prints every non-null key), so it needs no change,
  and absence-at-zero is what keeps the fields off ordinary reads.

## 3.6 As built

- `cursor_late_lost` counts a late record lost iff it left the ring before the cursor's MARK passed
  its number. A late record stored ABOVE the cursor's position that leaves after the mark passed it
  is an ordinary record above the position, and its loss is `evicted_before_window`'s to report.
  The first model test counted "every late record never delivered" and disagreed with the store by
  exactly those records; numbering late records in the model the way the store does made the two
  agree across every run.
- The dropped-commit guard (§3.4) is caught only by the `logs.export` probe test (a late-only
  reply), and the "mark jumps past a dropped late record" mutation only by the model test — each
  mechanism has at least one test that fails without it (controls C23a–C23j).
- **The pre-merge gate found what this spec missed: a CREATION FLOOR.** A cursor created by
  `bookmarks.add` sits at the seq counter; a record a filter kept out BEFORE the add is still in
  the pre-trigger buffer, and a trigger firing after the add flushes it late with a fresh number —
  so the late part (number above the mark, seq at or below the position) returned it to a "from
  now" cursor. §3.1's sufficiency argument was about records the cursor had read past; this one it
  never had. Every cursor now carries `floor` (`Bookmark::floor`, `CursorPos::floor`): the
  `bookmarks.add` start seq, 0 for an auto-created or restored one; the late part takes only
  `seq ∈ (floor, position]`. The same floor gives an explicit `start_seq` its plain meaning.
- An auto-created cursor's mark is the store's late counter, not 0 (`resolve_bookmarks_at`): at
  position 0 every held record reaches it through the normal part anyway, and a mark of 0 made its
  first read report every late record that ever left the domain as `cursor_late_lost`.
- `cursor_late_lost` is an upper bound, not an exact count of the cursor's own losses: a departed
  record's seq and fields are gone, so neither the floor nor the read's filter can be applied to
  it. Documented as such, beside `evicted_before_window`, which is an upper bound for the same
  reason.
- `CursorCommit::commit` compares-and-sets: it moves the cursor only from the position the read
  started at, and re-inserts a missing cursor only when the SEQ moved. For a READ or a
  `bookmarks.add replace` of the same cursor this is defensive: a cursor is read and replaced
  only through its own session, a named session has one connection (the claim on reconnect is
  atomic — before the re-gate, two `session.start`s for one name could both succeed), and a
  connection handles one request at a time. It is NOT unreachable for removals: another session
  can `bookmarks.remove owner/name` or `bookmarks.clear` that session's bookmarks between a read
  and its commit, and the commit then re-inserts the cursor at its new position with no
  description — the same outcome as the sweep race it was written for, and the intent of the
  read that advanced it.
- A restored cursor's floor is its restored position, not 0: a `bookmarks.add` with a
  `start_seq` above the counter must keep excluding what lies below it after a restart.
- The normal walk checks `seq > position` itself, not only through the filter's `CursorSeq`.
- Docs: README §"`c>=` — read and advance", the skill's cursor lines (`skill/logmon.md:703`, `:707`,
  `:792`), the SDK README cursor section (`crates/sdk/README.md:723-737`: `cursor_advanced_to: None`
  no longer means "cursor unchanged"), the `traces.logs` comment (`rpc_handler.rs:2230-2233`),
  CHANGELOG.

## 4. Tests

Verification (each must fail against today's code — the cursor must have committed PAST the late
record before it is stored, or today's code returns it too):

- V1 the issue's scenario through the real pipeline: a session filter stores only `m=marker`; a WARN
  is kept out, then a marker is stored; a cursor reads the marker (position now above the WARN); an
  ERROR fires a trigger whose pre-window holds the WARN; the next cursor read returns the WARN and the
  ERROR with `cursor_late == 1`; a third read returns nothing.
- V2 the same through `traces.logs`, every record in one trace (the harness's path).
- V3 count-limited: 5 late records below the position and 2 normal records above; reads with
  `count=2` return 2 late / 2 late / 1 late + 1 normal / 1 normal, each record exactly once.
- V4 `logs.export` probe prefix: a read whose `count + 1` probe takes a record the handler then drops
  — (a) a normal one, (b) a late one — delivers that record on the next read.

Bug-bounty (adversarial):

- A1 a late record below an explicit `from_seq` on `logs.export` with a cursor is NOT returned.
- A2 a late record evicted before the read is not returned, is counted in `cursor_late_lost`, and does
  not stall the cursor.
- A3 `logs.clear` empties the late maps (no phantom candidates after a clear).
- A4 overlap: the cursor read a marker at seq 100; WARNs at 95 and 105 were kept out; an ERROR at 110
  flushes both — 95 comes through the late part, 105 through the normal part, each once.
- A5 `bookmarks.add` after late records exist: a `c>=` read on it returns none of them.
- A6 property test on `InMemoryStore` alone: random interleaving of on-arrival stores, late merges,
  evictions, clears and cursor reads with random limits and random kept prefixes; everything
  delivered is distinct, and every record stored is delivered once unless it left the ring first
  (then it is counted lost if late). After every step `late.len() == late_by_seq.len()` and every
  late seq is held.

## 5. Not in scope

- Concurrent reads of the SAME cursor from two calls can double-deliver today; unchanged.
- Span cursors do not exist (`traces.recent`/`traces.get` reject `c>=`).
- A cursor positioned by `bookmarks.add` sits at the seq COUNTER, which may be above everything held.
  A record whose seq was assigned just before that and is stored just after (the one record a domain's
  processor can have in flight) is on-arrival, not late, and is missed — a pre-existing race of
  bookmark creation, not of late storage. A cursor positioned by its own reads is not exposed (§3.1).
- `cursor_advanced_to` is absent when only late records came back (the seq position did not move);
  readers count records, not this field (the Store harness reads `logs` only).
