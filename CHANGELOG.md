# Changelog

Notable changes per release. Versions are `0.x`, so the MINOR component carries
anything behaviour-visible; PATCH is reserved for fixes nobody has to know about.

## 0.6.0 — 2026-07-30

Comparison and guards — phases 4 and 5 of the span time collector design. The
driving workflow now closes: arm, run, snapshot, change one thing, run again,
snapshot, then ask what moved.

### Added

- **`collectors.diff`** — the only place logmon subtracts. An *arm* is
  `<collector>` (the live window), `<collector>@<label>` (one recorded run), or
  `<collector>@*` (every recorded run merged). The wildcard is not a
  convenience: with single-run arms on both sides every threshold in the result
  reads `unknown`, so a merged arm is the only shape whose deltas can be told
  apart from scheduling noise.

  Most of its behaviour is the cases where it *refuses*, in three severities.
  **Marked** — a level difference (compared at the lower one, with nesting
  evidence carried across regardless), a filter that differs only in spelling,
  one truncated arm. **Blocked with a named flag** — arms that matched different
  populations, one arm that lost spans while the other did not, both arms
  truncated. **Refused outright** — mismatched sketch layouts, where the
  subtraction would be arithmetic on two different scales and there is nothing
  to permit.

  Every row carries **the threshold that was applied to it**, in the metric's
  own units. Estimated percentile rows also carry `α(a+b)/|a−b|`, the worst-case
  error as a percentage of the delta — which reaches ±199% for a 1% change, and
  crosses 100% (the error bar as wide as the delta) below roughly a 2% relative
  change. Count rows and duration rows get **different** run-to-run floors: a
  fixed-iteration suite has near-zero count variance while its timings vary by
  percent, and thresholding one against the other over-suppresses badly.

- **`collectors.document`** — writes a measurement up for someone who was not
  there: what moved, what to do next, and every caveat beside the number it
  qualifies. `md` (default), `json`, or `folded` for a flame graph. The daemon
  returns bytes and the client writes them, as with `logs.export`. Regeneration
  is free and lossless, so nothing is stored and `finding` normally arrives on a
  second call after the first read.

  The document went to a **cold reader** — a reviewer given the rendered
  markdown and nothing else, no spec and no source — which found eight things
  the author could not see, including one message that was true of an exact
  count and false of an estimated percentile. Only the limitations that actually
  apply are listed, each with the remedy that would clear it, and a per-metric
  table says which numbers each one reaches. Absent numbers read `n/a` rather
  than being given a clean bill of health.

- **Threshold triggers** — `add_collector(threshold={metric, op, value,
  window_ms, group?})`, over `count`, `total_ms`, `avg_ms`, `error_count` or
  `error_rate_pct`. Evaluated against a rolling bucket ring **advanced by span
  arrival, never by a clock**, which is what keeps an idle collector free.

  The consequence is stated rather than left to be discovered: with no traffic a
  breached threshold neither fires nor clears. It is a load-time guard, not a
  liveness check — a downward threshold detects a drop *while traffic continues*
  and does nothing if traffic stops. Every report carries a note saying so.
  Percentiles are refused, because a rolling percentile needs a duration sketch
  per bucket and per-collector memory is bounded on purpose.

- **Snapshots record their top call paths**, so `folded` output works on a
  recorded run and not only on a live one. Two caps, because either alone leaks:
  200 rows bounds a pathological fan-out and 64 KiB bounds pathologically long
  paths. Truncation is reported — nothing about looking at a flame graph reveals
  that mass is missing from it.

### Changed

- Call paths are stored as **frames** rather than a joined string. A span named
  `parse > eval` is indistinguishable from two frames once ` > ` is the
  separator, and a flame-graph renderer that split the joined form back apart
  would emit a stack that never ran.
- The MCP surface grows to **42 tools**.
- `PROTOCOL_VERSION` stays at **1** and `FORMAT_VERSION` at **1**: every change
  here is additive, and the only version gate refuses a file newer than the
  running build — so an optional field reads correctly in both directions, where
  a bump would have made every existing snapshot unreadable for nothing.

### Fixed

- `collectors.edit` accepted a `threshold` change, reported `zeroed: true`, and
  kept the old limit — the new definition never received it.
- A threshold verdict was rendered as a JSON literal, emitting
  `"last_value": null` where the schema promises the key is absent. Absent
  versus null is the whole distinction that field carries.

## 0.5.1 — 2026-07-30

Nine defects found by the pre-merge adversarial gate on 0.5.0, all in the
collector work. Three finders over the frozen diff: line-by-line, cross-file
tracing, and a mutation-based test-validity pass.

### Fixed

- **A large `group_keys` list could stop the daemon.** The group-key column is
  reserved eagerly at `8192 × width × 4` bytes when a collector is built, and
  the width is the one dimension `max_sample_bytes` never covered — so a tiny
  declared budget with a huge key list passed every reservation check and then
  asked for gigabytes, which `Vec::with_capacity` answers by *aborting the
  process*. `traces.profile` was the widest surface: it builds its collector
  directly and never passes the reservation gate at all. Now capped at 8 with
  an error naming the number, checked on `add`, `edit` and `traces.profile`.

- **Collectors no longer leak the daemon-wide reservation.** Four ways they
  could, each leaving up to 64 MiB per collector held by something no RPC could
  see or release:
  - After a `kill -9`, collectors were restored but their owning session was
    not — session names only reach `state.json` on a graceful exit, while
    collector files are written through immediately. The owner is now
    re-registered at boot so the existing TTL sweep can reclaim them.
  - `session.rename` left `Entry::owner` behind, orphaning the renaming
    session's own collectors. They now move with it.
  - `session.drop` released collectors *after* dropping the session, so the `?`
    aborted when the session was already gone — precisely the state a
    restored-after-crash collector is in. Reordered.
  - Rename *displacement* handed a dead session's collectors, including their
    full recorded history, to whoever took the name. They are now cleared
    alongside that session's bookmarks, which the code already claimed.

- **Persistence no longer writes under the ingest lock.** Every collector write
  did two `fsync`s while holding the registry lock that `ingest_span` needs on
  every domain — so one slow disk could stall span ingest daemon-wide until the
  channel overflowed and dropped spans. Bytes are now serialized under the lock
  and written outside it. (`edit` keeps its persist-before-mutate ordering by
  re-acquiring and re-validating.)

- **`edit --domain` now carries the metrics handle.** Ingest attribution turns
  on pointer equality of that handle, so a re-pin that moved the name but not
  the counters reported *"the pinned domain is gone or was recreated"* about a
  domain that existed and had just been re-pinned to — on the design's own
  documented repair path for an orphaned collector.

- **The boot temp-file sweep now reaches collector files.** It was
  non-recursive, and collector temps live one directory down, so the files this
  feature added were exactly the ones it missed.

- **A large `skip_warmup_ms` no longer silently does nothing.** The cutoff used
  unchecked `i64` addition; with no overflow checks in release it wrapped to a
  large negative value, excluding nothing.

- **A mutual parent cycle no longer corrupts self time.** Only direct
  self-parenting was guarded, while the path walker already defended against
  cycles on the same data.

- **`snapshot` can no longer lose the run it just took** if the collector is
  removed concurrently — the window is already swapped out by then, so it is
  returned rather than turned into an error.

- **A corrupt collector file with inconsistent counts is refused** rather than
  underflowing `well_formed_count` into a nonsensical average.

### Added

- Three tests closing gaps the mutation finder proved were real: a child span
  entirely outside its parent, `overlapping_child_spans` when nothing was
  clipped, and a structural `edit` refused by the reservation.

## 0.5.0 — 2026-07-30

`PROTOCOL_VERSION` stays at **1**, as in 0.4.0: new methods and new fields on
existing results, so a 0.4.0 client keeps working against a 0.5.0 broker.

### Added

- **Snapshot history.** A `reset` throws a run away; a snapshot keeps it and
  starts the next window in the same call. `snapshot_collector`,
  `get_collector_history`, and `get_collector(snapshot=…)` — so the shape of a
  before/after comparison is: arm, run A, snapshot, change one thing, run B,
  snapshot, compare.

  **A snapshot carries its own definition** — the filter, level and group keys
  as they were when it was taken, not a pointer to the live collector's.
  Without that, a comparison reads *today's* definition and presents it as
  proof of what the recorded run measured, which is wrong exactly when someone
  edited the collector between runs.

  Sample-derived figures are computed at snapshot time or never, because the
  raw samples are not retained. `merge` combines the exact tiers and sketches
  across runs and reports a run-to-run spread; for a **single** run it reports
  that spread as *unknown* rather than zero, since zero would license calling
  any difference significant.

- **Collectors survive a restart.** One file per collector holding definition
  *and* history, written through at arm, edit and snapshot time rather than at
  shutdown — a definition that only reaches disk on a graceful exit is one a
  `kill -9` loses, leaving snapshots with nothing to attach to. The duration
  sketch round-trips, so recorded percentiles still work after a restart.

  A collector comes back **armed but zeroed**, and says so (`zeroed_by:
  "daemon_restart"`): a live window is a partial measurement interrupted by a
  restart, and resuming it would report a `wall_ms` spanning an outage with a
  span count that skipped it. History is unaffected.

  A collector pinned to an API-created domain is reported `orphaned` after a
  restart — those domains are not re-created, so this is the normal outcome
  rather than an exception, and the result carries the remedy.

- **`edit_collector`.** Editing the description is free. Editing the filter,
  level, `group_keys`, `max_sample_bytes` or domain **discards the live
  window** and re-runs every gate arming does — a window and the definition
  describing it must not disagree. Recorded snapshots are never touched. Levels
  move in both directions: dropping `tree` to `timing` buys 2.5× the retained
  records, which is the only remedy left once the sample budget is exhausted.

### Fixed

- **A corrupt `state.json` no longer stops the daemon from starting.** It was
  written with a plain `fs::write` and read with a `?` on the parse, so an
  interrupted write — a crash, a full disk — meant the broker refused to start
  until a human found and deleted the file by hand. A recoverable loss of
  session bookkeeping became an outage of everything.

  Durable writes now go temp → fsync → rename → fsync the directory, so a
  reader sees either the old file or the new one and never a prefix of the new
  one over the old. A boot sweep clears temps a crash left behind. On load, an
  unreadable file is moved aside (renamed, not deleted — it is the only
  evidence of what went wrong) and the daemon starts empty, saying loudly what
  was lost.

## 0.4.0 — 2026-07-29

`PROTOCOL_VERSION` stays at **1**. Everything here is additive on the wire —
new methods, new fields on existing results — and the handshake compares the
protocol version for exact equality, so bumping it would refuse every client
that had not been upgraded in lockstep. An 0.3.0 `logmon-mcp` shim talks to an
0.4.0 broker without noticing; it just won't offer the new tools.

### Added

- **Span time collectors.** Arm a filter, run a workload, read aggregate
  timings — the measurement logmon could not do before, when the only way to
  answer "did that change make it faster" was to eyeball individual traces.

  `add_collector` / `list_collectors` / `get_collector` / `reset_collector` /
  `remove_collector`, plus `profile_traces` for the same numbers over spans
  already in the buffer. Mirrored to the CLI as `logmon-mcp collectors …`.

  Three retention levels trade memory for what can be asked: `scalar` (counts
  and totals, no per-span records), `timing` (adds percentiles, wall-clock
  union, warm-up exclusion), `tree` (adds self time, nesting detection and
  call paths). `group_keys` splits every number by a span attribute, which is
  how both arms of an A/B run in one pass — and it reads attribute values
  directly, so a boolean kill-switch works, which it does not on the filter
  path.

  A result reports three categories that are **not** three views of one
  number: `exact` covers every matched span for the collector's life,
  `estimated` covers the same population to ±1% via a sketch, and `sampled` is
  exact over retained records — the whole population only while `complete` is
  true. Anything that cannot be computed is `null` with a named reason and
  usually a remedy, because `null` and `0` are different claims.

  Self time uses a clipped interval union, not a sum of children. A 100 ms
  parent with two concurrent 60 ms children has 40 ms of self time; summing
  gives −20, and clamping that to zero reports a parent that did no work of
  its own — on exactly the `tokio::spawn` workload the feature exists to
  measure.

  Cost, measured: about 0.21 µs per armed collector against a 4.10 µs ingest
  baseline, or 20% at the four-collector reservation ceiling.

- **Span loss is now counted.** `ReceiverMetrics` gained per-transport counters
  for whole batches shed under backpressure and for spans rejected at parse.
  Neither was visible before: HTTP sheds whole request bodies at 80% channel
  occupancy and returns 429 *before* parsing anything, so no per-span counter
  moved; malformed spans bumped a gRPC field nothing read, and on HTTP were not
  counted at all. A collector reports these as a window delta so a run can say
  whether it lost spans while it was measuring.

### Fixed

- **`get_slow_spans(group_by="name")` aggregated a doubly-biased sample.** It
  grouped the output of the slow-span query, which filters by a duration floor
  and then truncates to `count` — so `avg_ms` was the mean of the slowest few
  above a floor, presented as the mean for that span name. A name that ran
  10 ms a hundred times and 500 ms three times reported 500.

  It now aggregates the full matching population, and `min_duration_ms` is a
  display floor selecting which *names* appear rather than which spans count.
  New `population`, `display_floor_ms`, per-row `max_ms` and `p50_ms` make the
  basis visible: a low `avg_ms` beside a high `max_ms` means "usually fast,
  with a tail", which the old shape could not express. `p95_ms` also follows
  the lower-quantile convention — the previous `floor(n × 0.95)` returned the
  maximum at n = 20. The ungrouped arm is unchanged: a list of the slowest
  spans is not an aggregate, and truncating it is what it is for.

- **Span triggers re-parsed their filter for every span.** Evaluation went
  through the filter *string*, so each bound session re-parsed — and
  recompiled the seeded regex — once per span. Per-span ingest cost at four
  bound sessions was 118.6 µs; it is now 4.66 µs, and nearly flat in session
  count rather than linear.

## 0.3.0 — 2026-07-25

### Fixed

- **A trigger firing no longer blinds the other triggers in its session.** The
  post-window was a single counter on the session, and the log processor skipped
  evaluating that whole session while it was positive. For `post_window` entries
  (200 by default) after ANY trigger matched, NO trigger in that session was
  evaluated — a session-wide duty cycle of at most one firing per 200 entries.

  The effect was backwards from what you want: a frequently-matching trigger
  starved the quiet ones, and quiet triggers are exactly what you arm to catch
  something rare. Observed in practice with the built-in `l>=ERROR` trigger
  firing throughout a 70-minute test run while a trigger armed for a rare event
  recorded zero matches, with matching entries confirmed in the buffer.

  Firing suppression is now **per trigger**: each debounces only itself. The
  session-level counter is unchanged and still governs storage (capture context
  after a fire, bypassing filters).

- **A short `post_window` no longer truncates a longer one already in flight.**
  Now that a match can land inside an open window, the storage window extends
  rather than being overwritten.

- **`edit` applies to a window already in flight.** Shrinking `post_window` —
  notably to `0`, the documented way to ask for "count every match" — took
  effect only after the old window drained, which on a quiet stream could be
  indefinitely. Changing a trigger's *filter* now also re-arms it immediately.

- **Rebinding a session to another domain clears in-flight debounce windows**,
  completing the existing F3 invariant for firing as well as storage.

- **Span triggers now count their matches.** A span trigger's `match_count`
  stayed `0` forever, so "has this ever fired?" was unanswerable.

### Added

- **`post_remaining` on `TriggerInfo`** (`triggers.list` and `triggers.edit`;
  `triggers.add` returns only the new id, as before):
  entries still to pass before a trigger can fire again; `0` means armed and
  live. If a trigger looks stuck, this distinguishes "debounced" from "broken".
  Additive and defaulted, so older clients are unaffected.

### Changed

- **Expect more notifications** in sessions with several triggers, and
  `match_count` to start moving on triggers that previously read `0` for their
  entire lifetime. Those fires were being silently dropped; the new numbers are
  the corrected ones, not a regression.
- **Expect your filters to be bypassed more often.** The storage post-window
  is re-armed by any trigger firing, and triggers now fire that previously
  could not, so more entries are stored unconditionally as post-trigger
  context. Bounded — the window still closes within `max(post_window)` entries
  of the last fire, and the store is a fixed-size ring — but in a session with
  several frequently-matching triggers it can stay open continuously.
- **A firing entry is far more expensive than a normal one** (~200 µs vs
  ~0.6 µs at `pre_window: 500`: a store scan plus context clones). That cost
  was previously rate-limited to once per `post_window` entries per session;
  per-trigger debouncing makes it reachable per trigger. An undebounced
  (`post_window: 0`) trigger on a busy stream can bottleneck that domain's
  processor — see the README's note before using it.

### Notes

- Span triggers still fire on **every** matching span and are never debounced,
  so their `post_remaining` is always `0` and carries no information. Unifying
  the log and span evaluation paths is deferred: span triggers always fire, so
  the gap costs extra notifications rather than silent misses.
- Documentation for `matched_filters` was corrected. It is populated only when
  `source == Filter`; an entry stored because a trigger fired (`PreTrigger` /
  `PostTrigger`) matched no filter, so an empty value there is correct. Read it
  together with `source`, never alone.
