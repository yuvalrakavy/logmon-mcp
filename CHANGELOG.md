# Changelog

Notable changes per release. Versions are `0.x`, so the MINOR component carries
anything behaviour-visible; PATCH is reserved for fixes nobody has to know about.

## Unreleased

### Changed — the shim is now built from the broker's manifest

**Upgrade the broker BEFORE the shim.** The shim now requires `tools.manifest`
and refuses to start without it; a broker older than that leaves you with no
tools at all. Install `logmon-broker` first, confirm it is serving, then
`logmon-mcp`.

Both surfaces are assembled at startup from what the broker declares, so a tool
added to the broker becomes an MCP tool and a CLI command with no reinstall.
The 45 `#[rmcp::tool]` attributes, the 37 parameter structs and the ten
hand-written CLI command groups are gone.

CLI commands are now **derived from RPC method names**, which renames a few
things. It also surfaced a long-standing naming inconsistency, fixed in its own
entry below rather than absorbed here:

| was | is |
|---|---|
| `collectors profile` | `traces profile` (the method is `traces.profile`) |
| `--group-key a --group-key b` | `--group-keys a --group-keys b` |
| `--threshold-metric` / `-op` / `-value` / `-window-ms` | `--threshold.metric` / `.op` / `.value` / `.window-ms` |
| `logs export --out FILE` | `logs export --path FILE` (`--path -` for stdout) |
| `collectors snapshot --no-reset` | `collectors snapshot --reset false` |
| `--session` / `--domain` anywhere | before the command only — after it they belong to the tool (`collectors edit --domain` re-pins a collector; `bookmarks list --session` reads another session's) |

`triggers add` now takes the broker's defaults (pre 500 / post 200 / notify 5)
when those flags are omitted, and `traces get` includes linked logs by default.
Both match what the MCP tools have always done — the CLI was the outlier,
forcing 0/0/0 and `false`.

Output is JSON unless the broker supplies a rendered form. The CLI's
hand-written renderers went with the command groups; presentation now belongs
to the broker.

### Changed — `session.*` is now `sessions.*` (wire change)

The one singular method group among ten plural ones, inconsistent since the
methods were introduced together in March. It stayed invisible while the CLI
carried a hand-written `sessions` alias; derivation removed the alias and put
the asymmetry on the surface.

`session.list` / `session.drop` / `session.rename` become `sessions.list` /
`sessions.drop` / `sessions.rename`, and the protocol types follow
(`SessionList` -> `SessionsList`, and so on) because the schema definition name
is derived from the method. The SDK wrappers rename with them:
`Broker::session_list` -> `Broker::sessions_list`.

**The CLI is unaffected — that is the point.** `sessions list` and
`sessions drop` are what they were before derivation; renaming the method is
what keeps them that way. Fixing this after release would have meant renaming
a command twice.

**MCP tool names do not change.** `get_sessions`, `drop_session` and
`rename_session` are independent of the method they call, and were already
plural. Agents see nothing.

Direct RPC and SDK callers must move. There is no alias: a shim and a broker
are installed as a pair, and an alias kept for a transition nobody is running
is a second name to support forever.

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

  **`collectors.add` takes a `data` shorthand**, defined as
  `domain_data.update(entries)` on that call's domain — the same validation, the
  same per-entry outcomes, the same `/logmon/` guard, the same caps, one
  implementation. Not a namespace, not a parallel store.

  It exists because a convention that lives only in documentation is a
  capability nobody reaches for. Recording what the project was *at the moment
  you are already describing what you are measuring* costs one parameter;
  recording it in a separate call costs a **decision**, and the decision is the
  part that does not get made.

  It is a shorthand and nothing more: not persisted with the collector, absent
  from its definition, untouched by `collectors.edit` — the registry already
  holds it. `@`-sigilled keys are refused, because the sigil scopes a fact to one
  document and arming a collector writes none. The entries are applied only once
  the collector is actually armed, so a refused call leaves nothing behind that
  its caller never saw an outcome for. The reply carries `data_outcomes` only
  when there is something to report.

- **`logs.export` takes a seq range.** `from_seq` and `to_seq`, both
  **inclusive**, for reading a bounded window rather than the newest N.

  Inclusive on both ends because the alternative has an off-by-one exactly where
  it hurts: a window built as "this entry, plus none before and none after" must
  contain that entry, and a strict bound would drop the one record the caller
  was pointing at. Internally they lower to `Gt(from - 1)` and `Lt(to + 1)`,
  saturating so the adjustment cannot wrap.

  **The range is folded into the parsed filter rather than carried beside it**,
  which is what makes it honest. `evicted_before_window` reads its lower bound
  out of the parsed filter, so a range passed as a loose parameter would be
  invisible to it — and a window whose start had already rolled out of the ring
  would come back `truncated: false`, telling a reader the window was complete
  when part of it was gone. Folding also makes the range compose with a bookmark
  bound for free: the effective lower bound is the max of all of them.

  **`capped` is new on the result**, stating whether `count` stopped the export
  short of everything that matched. It cannot be inferred — "asked for N, got N"
  is true both when a range was cut and when exactly N existed — and it is a
  different kind of incompleteness from `truncated`, which reports eviction.

- **`spans.export` — a seq-ranged span read, which did not exist.** The span
  store's entire surface was `get_trace`, `slow_spans`, `for_each_matching`,
  `duration_by_name`, `recent_traces` and `spans.context` — and `spans.context`
  counts *positions in the ring*, not sequence numbers, so nothing could answer
  "which spans belong to this window". Pairing spans with the logs over the same
  interval needs exactly that.

  Takes `from_seq` / `to_seq` (inclusive, same convention as `logs.export`),
  an optional `count`, and an optional `filter` ANDed with the range. The range
  is folded into the filter by the same code path `logs.export` uses, so the
  bounds, the saturating adjustment and the eviction detection cannot drift
  between the two.

  **The verdict it reports is the span ring's own.** Logs and spans share a
  sequence axis but evict independently, so a log window reported complete says
  nothing about whether the spans covering it survived — `buffer_oldest_seq`,
  `truncated` and `evicted_before_window` are all computed against the span
  store. One verdict cannot honestly cover two stores with independent
  retention.

  `capped` is exact rather than inferred here: the store reports how many spans
  matched in total, so the reply carries both `count` and `matched` and says by
  how much it fell short. A cursor qualifier is refused — it is read-and-advance,
  and gathering evidence must not move the caller's read position.

- **`logs.export` says how much of the window it can vouch for.** Two new
  fields: `verdict`, one of `complete` / `evicted` / `filtered` /
  `cannot_verify`, and `narrowed_by`, which names the filters and the seqs when
  the answer is `filtered`.

  The problem it solves: storage is conditional. When any session bound to a
  domain holds a filter, only matching entries are kept — so *"nothing appeared
  before the error"* is ambiguous between **absence of cause** and **absence of
  recording**, and only the first is a conclusion. Until now nothing on the wire
  could tell them apart, and the reading a caller would naturally take is the
  wrong one.

  Reading the live filter set at export time cannot answer it either, because
  filters are time-extended and the read is a point. An anonymous session is
  removed on disconnect and takes its filters with it, so a capture taken
  minutes after the run sees nothing and would assert that nothing was
  narrowing a window that recorded a tenth of it.

  So the daemon now keeps a **per-domain epoch log**: the first seq decided
  under each storage policy, plus that policy. A window is `complete` only when
  it lies wholly inside one unfiltered epoch, with nothing evicted and nothing
  capped.

  Three things about it are worth knowing:

  - **It records the filter *strings*, not a boolean.** `filters.edit` replaces
    a condition in place and never moves a boolean, so a flag-only marker would
    report the new filter string over the old range — wrong information rather
    than missing information, which is the failure the whole feature exists to
    prevent.
  - **The boundary is stamped by the processor, from the policy that decided
    the entry** — not beside the mutation that caused the flip. The seq is
    assigned at the top of the processing loop and storage is decided at the
    bottom, so a stamp taken elsewhere would be approximate by whatever is in
    flight, and `complete` would be claimed over entries the other policy
    actually judged.
  - **A disconnected named session's filters still narrow the store**, and the
    verdict reflects that, because the policy is read from the same scan that
    decides storage rather than from a separate "who is live" question.

  `cannot_verify` is a verdict rather than a caveat, and it is the **default**
  when the field is absent: an empty store satisfies every clause of `complete`
  vacuously, and so does a reply from a broker too old to send the field. A
  window carried over from a previous daemon run reports it too — the log opens
  at the seq this incarnation started from, so a restored domain cannot speak
  for its predecessor's seqs.

  Nothing changes for spans: they are stored unconditionally, so `spans.export`'s
  own eviction reporting is already the whole answer for that store.

### Fixed

- **A parameter of the wrong type is now an error, not a different answer.**
  Every RPC parameter was read with `params.get(k).and_then(|v| v.as_TYPE())`,
  and `as_TYPE` returns `None` both when a key is **absent** and when it is
  **present with the wrong type**. Nothing downstream could tell those apart, so
  the wrong type did not produce an error message — it produced *a different
  operation*, reported as success.

  Where the default merely narrowed a query, the caller got more than they
  asked for: a wrong-typed `filter` meant **no filter**, so `logs.recent` with
  `{"filter": {"expr": "level>=ERROR"}}` returned the entire buffer. Where the
  default was `true`, it was worse. `{"reset": "false"}` — a stringified
  boolean, the commonest client mistake there is — read as absent, took
  `reset`'s default of `true`, and **discarded the collector's live window**. A
  wrong-typed `group_keys` armed an *ungrouped* collector and persisted it,
  holding a 64 MiB slice of a daemon-wide reservation that fits about four; on
  `collectors.edit` the same value counted as a structural change and zeroed the
  window. A wrong-typed `session` on `bookmarks.clear` cleared the **caller's
  own** bookmarks instead of the ones they named.

  The rule now, everywhere: **absent — or an explicit `null` — takes the
  default; a present value of the wrong type is refused, naming the parameter
  and what was expected.** Absent ≡ `null` is the rule `domain_data` already
  states for the same reason: a client that serialises an omitted field as
  `null` must not thereby mean something different from one that omits it. An
  explicitly empty array stays legal and stays distinct from a wrong type, which
  is what keeps `group_keys: []` a deliberate clearing rather than an accident.

  **One of these was already firing, through logmon's own shim.** The MCP
  server builds its params over `Option` fields, and `json!` renders `None` as
  `null` rather than omitting the key — so every `edit_collector` call sent
  `group_keys: null`. The old reader checked *presence*, found the key there,
  read the null as an empty array, and handed the registry an explicit
  "no group keys". Against a collector that had any, that is a structural
  change: an agent editing only the description silently cleared its group keys
  and discarded its live window. No malformed client was needed; the shipped
  shim did it on the documented path.

  Two neighbouring conflations went with it. `"missing required parameter: X"`
  was reported for values that were present but wrongly typed, sending callers
  to look for a key they had already sent. And numbers were **truncated**
  instead of range-checked: `pre_window: 4294967296` became `0` — the largest
  window a caller could ask for, capturing nothing, reported as success — and
  `id: 4294967297` addressed filter `1`. A `before`/`after` context window
  larger than any possible buffer reached an unchecked `idx + after + 1`, which
  wraps in release and panics in debug inside the connection task; a
  `validated_before_secs` above `i64::MAX` seconds reached a `chrono::Duration`
  constructor that panics. Both are refused at the boundary, naming the bound.

  Error **codes** are unchanged: every handler failure still returns `-32601`
  with its message. Splitting parameter errors out to `-32602` is right and is
  worth its own change, but half a surface carrying a meaningful code is worse
  than a whole one carrying none — a client would start branching on a code that
  is only sometimes true.

- **A rejected `group_by` now names every value it accepts.** It named four of
  the five, and the one it left out was `none` — the only spelling for "the
  overall, ungrouped figures". A caller who trusted the error could not reach
  the ungrouped read from the very message refusing them, on `collectors.get`,
  `traces.profile`, `collectors.diff` and `collectors.document` alike.

  The schema was right the whole time, which is why nothing contradicted the
  text and it survived to be found by hand: `protocol-v1.schema.json` declares
  the full set and the agreement test checks it in both directions. The
  sentence a human reads was the last hand-written copy, so it is now derived
  from `GroupBy::ALL` / `DiffGroupBy::ALL` — which that test reads too. One
  enumeration, three consumers, nothing left to drift.

  Aliases stay out on purpose: `""` is `none` sent by a client with an empty
  field, not a distinct option. The same rule leaves `>` out of the
  threshold-op message and `markdown` out of the format one — both already
  name every canonical value, and both are unchanged.

- **`traces.slow` no longer swallows a `group_by` typo.** It accepted *any*
  string, and anything that was not `"name"` selected the ungrouped arm. So
  `--group-by nmae` returned a full, plausible, ungrouped result to a caller
  who had asked for grouping — no error, no warning, and nothing in the
  response to say the parameter had not been understood.

  It was the only `group_by` on the surface that behaved this way, and the
  leniency was deliberate: the stated reason was that a closed set *"would
  reject every valid call that means do not group"*. That held only while
  `traces.slow` had no spelling for "do not group". It has one now — `none`,
  the same as every other `group_by` — so the set closes without rejecting
  anything valid. Omitting the parameter, `none` and `""` all still mean
  ungrouped.

  This is a behaviour change for a caller currently sending an unrecognised
  string to mean "ungrouped": that call now fails, naming what it accepts. Read-
  only does not make a wrong answer safe, and a typo that silently changes the
  question is the failure the strict readers on this surface exist to close.

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
