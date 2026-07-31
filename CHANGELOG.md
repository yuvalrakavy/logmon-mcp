# Changelog

Notable changes per release. Versions are `0.x`, so the MINOR component carries
anything behaviour-visible; PATCH is reserved for fixes nobody has to know about.

## Unreleased

### Added

- **`domain_data` — a per-domain provenance registry.** A small key/value store
  recording what was true of the project while the logs were produced. Logs
  without it are a dump; logs with it are evidence, and six months on that is the
  whole difference. Three tools: `update_domain_data`, `get_domain_data`,
  `remove_domain_data`.

  **Two timestamps per key, not one.** `created_at` is when *this value* came
  into force; `validated_at` is when someone last confirmed it. A single
  "modified" stamp conflates two different questions — set six days ago and never
  revisited is a guess, the same value confirmed five minutes ago is evidence —
  and only the pair can tell them apart. So sending a value that has not changed
  reports `validated`, not `updated`, and moves only the confirmation.

  **Sending a key with no value confirms it rather than creating it.** Inventing
  a valueless key is the helpful guess that becomes wrong data, so a key-only
  entry against a missing key reports `unknown` with a cause: `never_set` when
  the registry is there and the key is not, `undetermined` when there is no
  registry and nothing establishes why. The third case exists because those two
  really are indistinguishable from the registry alone, and claiming the first
  for both would be a guess dressed as a fact.

  **An optional `ttl` per key** (`30s`, `5m`, `2h`, `7d`, `4w`; `false` clears
  it), measured from the last confirmation. Without one, a key reports its age
  and **no verdict** — deliberately. logmon will not tell anyone a value is
  current on the strength of a duration nobody stated.

  **`/logmon/*` is logmon's own** — the domain, the broker version, when the
  registry was first seen, and which era of a reused domain name this is. Agents
  cannot write there, and cannot remove it either: the reservation is over path
  *segments*, so the bare `/logmon` is refused as firmly as `/logmon/version`,
  in both directions. A `remove` pattern that would take the subtree with it is
  refused for the same reason, because `remove` has no undo.

  **Its own file per domain, in its own subdirectory**, written whole and
  atomically. The subdirectory is not cosmetic: `config` and `state` are legal
  domain names, and the daemon's own `state.json` parses any JSON object
  successfully — a flat layout could silently cost every named session, trigger,
  filter and bookmark on the next boot.

## 0.9.0 — 2026-07-31

Makes capability skew visible. On 2026-07-30 a project filed a report proposing
three collector features that already shipped — their MCP shim was several
versions behind the broker, and nothing anywhere said so. Both the tool list and
the skill file are compiled into that binary, so a stale shim is silent by
construction.

### Added

- **`status.get` now reports `broker_version` and `broker_tools`** — the MCP tool
  names a shim built at this broker's version exposes.

  This is deliberately on `status.get` rather than the handshake, because
  `get_status` relays the broker's JSON **verbatim and always has** — every shim
  ever built renders unknown fields untouched. So this reaches installations
  already in the field: a stale shim shows the new facts after a broker restart
  and nothing else. Tool names rather than RPC method names, because that is the
  vocabulary an agent can compare against what it is holding
  (`traces.slow` is `get_slow_spans`; `collectors.reset` is `reset_collector`
  but `collectors.document` is `document_collectors` — no derivable mapping).

- **A `shim_note` in the same response** when the shim is missing tools the
  broker supports, naming them and the reinstall command. Silent when the sets
  match, when the broker is too old to advertise, and when the shim is *ahead*
  of the broker — absent is unknown, not "you are missing everything", and a
  notice in the everyday case is what teaches a reader to ignore it.

- **`logmon-mcp status` prints the broker version**, and says so when it differs
  from the CLI's own. That is the surface a human is looking at while deciding
  whether to reinstall.

### Internal

- `protocol::mcp_tools::TOOLS` — one `(tool, method)` table, read by the broker
  to state its inventory and by the shim to diff against what it exposes. A test
  pins it to the router `#[rmcp::tool]` actually generates, so the mirror cannot
  drift; another asserts every method in it is one the broker dispatches.

### Known limitation

`status.get` resolves the caller's domain first, so if that domain has been
deleted the call errors and these facts go with it. Pre-existing, reachable only
via `use_domain(x)` then `delete_domain(x)`, and the error names its own remedy.
Serving a partial status would mean letting an absent buffer read as an empty
one, so the strict field stays. Pinned by a test rather than left to be found.

### Compatibility

Additive. `PROTOCOL_VERSION` stays 1, `FORMAT_VERSION` is untouched, and nothing
is added to the session handshake. An older shim renders the new keys verbatim;
an older broker omits them and the shim stays silent.

## 0.8.0 — 2026-07-30

Acting on the first production use of the collectors, by a session that had no
hand in designing them. The arithmetic held up under it — one arm reproduced a
figure recorded days earlier, on a different broker version, to within 0.8%.
What did not hold up was the *labelling*, and two of the fixes below mean some
numbers you have already read were not measuring what they said.

**Re-check two kinds of past reading** (see Fixed for the mechanism):

1. Any **snapshot** read with `skip_warmup_ms` — the cut was never applied, so
   those figures include the warm-up period while claiming not to. They are
   biased high, and the per-span records they came from are gone, so the run has
   to be repeated rather than re-derived.
2. Any **live** read combining `skip_warmup_ms` with `group_by: name` or
   `group_by: group` — the headline figures excluded warm-up, the per-row
   breakdown did not, and comparing the two would understate how much of the
   total the warm-up span accounted for.

Everything else here is additive and changes nothing you have already
recorded.

### Added

- **`sampled.durations_ms`** — every retained duration, in **arrival order**.

  *Present when* the collector retains per-span records (level `timing` or
  `tree`), the sample was not truncated by `max_sample_bytes`, and it holds **at
  most 50** of them. Above 50 the list is withheld and the percentiles and
  spread stand in for it.

  The 50 gates what is **stored**, not merely what is printed: a snapshot keeps
  the projection as it stood when taken, so a run that exceeded the cap has no
  durations to recover later. This suits a collector that matches once per run —
  three runs is three records — and not one that matches once per iteration.

  Arrival order rather than sorted because a reader can sort them but cannot
  unsort them, and the order carries drift across a run — first-call effects, a
  cache filling — that every other figure discards. **`durations_ms[0]` is the
  first duration, not the smallest.** The list covers the same population as the
  percentiles beside it, after any warm-up cut.

- **`sampled.stddev_ms`** — sample standard deviation, Bessel-corrected, over
  the same population. Absent below two records, where it is undefined rather
  than zero, and absent on `group_by: path` rows, which keep a count and a
  self-time sum rather than the durations it would need.

  The `n-1` form because the population form is about 18% smaller at three
  records. That is the gap between the two *formulas* — both still estimate the
  true spread from very few samples, and at three records the estimate is itself
  uncertain to roughly ±45%. Treat it as a description of the observed spread,
  **not a significance test**: separating two three-run means properly takes a
  difference of roughly 2.3 standard deviations, not one.

  Both fields live on the sampled block, so they are **recorded into snapshots**.
  A run captured from 0.8.0 onward carries its own durations permanently *when
  it met the conditions above* — complete, and at or under the cap.

- **`excluded_by_warmup`** — how many retained spans `skip_warmup_ms` removed.
  **Absent means no count could be produced**, never zero: either no cut was
  asked for, or none could be positioned because the level retains no spans to
  measure from. "Warm-up was negligible" and "warm-up was never cut" are
  opposite facts about the same number. Counted once, off the same record set
  every filtering view walks, so no two views can disagree about one filter.

- **`groups_total`** on a profile — group keys before `top_n` truncation (which
  defaults to 20), so a reader can tell the top 20 of 20 from the top 20 of 900.
  Absent when no grouping happened, including when one was asked for and
  refused: a refused grouping has no denominator, and `0` would read as "this
  run touched nothing". `collectors.diff` has carried a field of this name since
  0.6.0; it counts only *comparable* keys, so on an axis at its cardinality cap
  the two can differ by one.

### Changed

- **`group_by: name` and `group_by: group` are now withheld when a warm-up cut
  actually runs**, with a `suppressed` entry saying why. Those rows are built
  from accumulators written at ingest, which have no window — so they were
  handing back at full weight exactly the spans the read had excluded, next to
  headline figures that excluded them, which is the same reason `exact` and
  `estimated` are already withheld under a cut.

  Withheld on the cut *running*, not on `skip_warmup_ms` being passed: where no
  cut could be positioned — `scalar`, or a window that retained nothing — the
  rows are identical to a read without the option and are served unchanged.
  `group_by: trace` and `path` are projected from the retained records, honour
  the cut, and are unaffected; they need level `tree`.

- **An unparseable `group_by` on a snapshot read is now an error**, as it always
  was on a live read. It was previously discarded in silence.

- **`collectors.history` no longer carries `durations_ms`.** The 50-record cap
  budgets one `sampled` block; a listing embeds one per run for up to 50 runs.
  Read a single run — `get_collector(name, snapshot=label)` — for its durations.

### Fixed

- **A recorded run silently ignored the options it could not honour.** Reading a
  snapshot with `skip_warmup_ms` or `group_by` accepted the parameter and served
  the stored numbers as though it had been applied — the projection is computed
  when the snapshot is taken, and the per-span records it came from are gone.
  Both now say so, as an entry in the response's `suppressed` list; **the call
  still succeeds and nothing else about the response changes.**

### Compatibility

Every wire and file change is additive. `PROTOCOL_VERSION` stays 1 and
`FORMAT_VERSION` stays 1: snapshots written by 0.8.0 load on older builds, which
ignore the new fields, and snapshots written by older builds load here with
`stddev_ms` and `durations_ms` absent — which reads as *not recorded*, never as
an empty list or a zero spread.

`excluded_by_warmup` and `groups_total` are computed per read and never stored,
so a snapshot recorded before 0.8.0 carries no stale claim about either.

One asymmetry worth knowing before you downgrade: an older build that loads a
0.8.0 snapshot **drops the two new fields on its next write**, permanently. The
data degrades rather than corrupting — nothing misreads — but re-upgrading will
not bring the durations back.

## 0.7.0 — 2026-07-30

Two deferred items from the span-collector design, and the twelve defects a
pre-merge gate found in them.

### Added

- **`LOGMON_CONFIG_DIR`** relocates the whole config and state directory —
  `config.json`, `state.json`, `daemon.pid`, `daemon.lock`, `logmon.sock`,
  `daemon.log`, `collectors/`. Read by the **daemon and its clients**, so one
  variable stands up a second broker beside a live one and any client in the same
  environment finds it.

  Three properties worth knowing, each of which exists because the alternative
  bit: it moves **state, not ports** (a broker in the new directory reads *that*
  directory's `config.json`, so with no file there it runs on stock defaults and
  collides — pass explicit ports, or put a `config.json` there); it must be
  **absolute**, because processes share an environment but not a working
  directory, and a relative value is ignored with a warning rather than pointing
  daemon and client at different places; and **auto-start refuses** in a
  relocated directory with no `config.json` instead of spawning a broker that
  would bind the default ports, naming the command to run instead. The managed
  service is unaffected — launchd and systemd do not inherit a shell's
  environment.

- **A real `kill -9` restart test**, out-of-process against the actual binary
  (`crates/broker/tests/kill_dash_nine.rs`). §12 objected to testing restart
  survival with a graceful `restart()`, because a graceful path writes on the way
  down and so passes whether or not the files were already complete. This kills
  the process cold and restarts on the same directory, asserting the definition,
  every recorded run, `zeroed_by: "daemon_restart"`, the threshold surviving with
  its rolling window cleared, and the auto-label counter continuing rather than
  re-issuing an evicted label. A second test proves the pid-liveness check
  excludes a second *process*, asserting the refusal's reason rather than merely
  that it failed.

- **`status.get` reports `trace_ingest`** — `dropped`, `shed_batches`,
  `malformed_dropped` — a **sibling** of `receiver_drops` rather than a member,
  because that field documents itself as counting *silent* loss and a shed batch
  is a 429 the caller saw. Scoped to the session's bound domain.

  **`dropped` is not new information**: it is exactly
  `receiver_drops.otlp_http_traces + otlp_grpc_traces`, the same two atomics,
  repeated so the three trace figures read as one block — so summing it with
  those fields double-counts. `shed_batches` and `malformed_dropped` are the
  numbers nothing else in the payload reports, and `shed_batches` counts request
  *bodies*, not spans: they were refused before being parsed, so how many spans
  they held is unknowable.

- `ThresholdInfo.effective_window_ms` — the window as evaluated, i.e. the
  declared width rounded up to a whole number of ring buckets, so a guard is
  never narrower than asked for.

### Fixed

- **Auto-start in a relocated directory silently broke every logmon tool.** The
  MCP shim's auto-start follows `LOGMON_CONFIG_DIR` but cannot pass port flags,
  so it spawned a broker that bound the *default* ports, collided with the live
  daemon, exited 1 with its stderr discarded, and left the caller polling a
  socket that would never appear — surfacing after ten seconds as "timed out
  waiting for daemon socket", naming neither the collision nor the variable. It
  now refuses with the command to run, and the child's stderr goes to
  `autostart.log`.
- A set-but-empty `LOGMON_BROKER_SOCKET` resolved to `""` and shadowed every
  later step in the socket chain, including the new fallback.
- A relative `LOGMON_CONFIG_DIR` was accepted by both daemon and client, which
  would have resolved to different directories. Both now ignore it, and the
  daemon logs why.
- An unrepresentable threshold recorded in a *snapshot* destroyed the whole
  recorded run on restore, where the same thing on a live collector only dropped
  the guard. A snapshot's threshold is inert metadata nothing evaluates.
- `path_aggregates` lost `incomplete` from its sort tiebreak, so a complete chain
  and a same-named suffix tied at equal self time and hash order decided — two
  snapshots of identical data could store different flame graphs.
- A ranked table grew a spurious "unattributed residual" row on large,
  fully-reconciling tables: the tolerance was absolute against f64 millisecond
  sums that each carry their own rounding.
- A `window_ms` that was not a multiple of the bucket count evaluated a window
  *narrower* than declared — up to 48% narrower at `window_ms: 31`, against a
  documented "at most 1/16". It rounds up now.
- `threshold.group` matched only the collector's first declared group key, so a
  value belonging to any other key silently never matched — indistinguishable
  from no traffic. Documented, and warned at arm time.
- `collectors.diff --group-by group` on collectors declaring no group keys
  returned an empty breakdown with no reason.
- Two SDK/daemon precedence claims in doc comments were wrong, and one asserted
  a check that did not exist.

### Testing

Two coverage gaps found by a mutation lens rather than by reading, both closed:
the SDK's socket resolution had **no** test (every connection in the suite passes
an explicit path, so deleting the env branch left the whole workspace green), and
the auto-label counter's restore was laundered by `reserve_label`'s
collision-avoidance loop below the retention cap — the real defect only appears
past eviction, where a reset counter re-issues a label whose original is gone.
Both now have tests that fail under the exact mutation that survived.

`verify.sh` runs the suite with `--all-features`, because a dozen integration
files are gated behind `test-support` and a plain `cargo test --workspace`
compiles them empty and reports them ok.

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

- `ThresholdInfo.effective_window_ms` — the window as **evaluated**: the
  declared width rounded up to a whole number of ring buckets, so the guard is
  never narrower than asked for and the difference is never something a caller
  has to infer.
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
- **Thirteen defects found by the pre-merge adversarial gate**, fixed before
  this release was tagged. The three worst would each have produced a confident
  wrong answer rather than an error: `<collector>@*` merged runs recorded under
  **different definitions** (now refused, naming both runs — §7.1 keeps history
  across a structural edit, so a history legitimately spans configurations and
  summing them reported the spread across configurations as scheduling
  variance); `reset` left the threshold's rolling ring loaded, so a re-pinned
  collector carried a verdict from the old domain's traffic; and the
  threshold's `avg_ms` divided by all matched spans instead of the spans that
  contributed to the sum, silently understating the average on dirty input.
  Also: `folded` could never succeed on a live arm; YAML front-matter broke on
  a multi-line `finding`; a flame-graph line broke on a control character in a
  span name; path ordering was nondeterministic at equal self time; a spurious
  residual row appeared on large reconciling tables; small `window_ms` values
  evaluated up to 48% narrow; `threshold.group` silently matched only the first
  group key (now documented and warned); an unrepresentable snapshot threshold
  destroyed the whole recorded run; and `group_by: "group"` with no group keys
  returned an empty breakdown with no reason. Full detail in commit
  `fix: thirteen defects from the pre-merge adversarial gate`.

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
