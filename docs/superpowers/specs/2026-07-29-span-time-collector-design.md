# Span Time Collector — Design Spec

**Date:** 2026-07-29
**Status:** DESIGN — awaiting user review, then the T2 design gate. Not yet built.
**Tier:** **T2** — novel feature, and it mints externally-binding contract surface
(new JSON-RPC methods, new MCP tools, new wire types in `protocol-v1.schema.json`).
Per the tier router, a contract outlives its code, so the contract goes through
staged design even where the surrounding machinery is pattern-following.

---

## 0. Motivation

logmon today answers *"what happened?"*. It does not answer *"where did the time
go, and did my change help?"*.

The driving case: measuring a cache added to a system under test by running the
full suite twice — once with the cache's kill-switch on, once off — and comparing
total time spent in a class of spans. That is exactly the **capability A/B** the
ways-of-working skill requires of any optimization behind a kill-switch, and today
it cannot be done from logmon at all.

The generalisation: a **span time collector** that accumulates spans matching a
filter and lets the consumer ask any timing question of the result — sum, average,
percentiles, self-time, wall-clock coverage, per-name and per-trace breakdowns.

This turns logmon from a diagnostic tool into a diagnostic *and* performance-analysis
tool, without adding a second data source: it is all derived from spans logmon
already ingests.

### 0.1 Design principle that shaped everything

**Collect broadly; decide at read time.** The collector does not compute metrics.
It retains a compact record per matched span, and *every* metric is a read-time
projection. This was a user-directed correction to an earlier draft that asked
which single summing rule to bake in (raw sum vs self-time vs wall-clock union).
Retaining `span_id` + `parent_span_id` makes self-time a read-time computation over
the matched set, which removes the ingest-time bookkeeping and the arrival-order
hazard that made self-time unattractive in the first place.

---

## 1. Current architecture — the seams we build on

Verified against the tree at `a08c119`.

| Seam | Where | Why it matters |
|---|---|---|
| Span ingest hook, per domain | [`process_span_for_domain`](../../../crates/core/src/daemon/span_processor.rs) | Already stores the span, then evaluates span-filter triggers for sessions bound to this domain. The collector is a sibling step in this function. Span triggers **already exist** — the README roadmap line "Span trigger evaluation (currently triggers only watch logs)" is stale and should be dropped in this change. |
| Span matching | `matches_span` / `matches_span_qualifier` (`crates/core/src/filter/matcher.rs`) | Reused verbatim. Supports `sn`, `sv`, `st`, `sk`, `d>=`/`d<=`, bare patterns, and arbitrary span **attributes** via `AdditionalField`. |
| Bookmark windows on spans | `matches_span_qualifier` `SeqFilter` arm | Bookmarks resolve to `SeqFilter` at the RPC layer, and spans/logs share one seq counter — so `b>=start, b<=end` **already works on span filters today**. `traces.profile` gets window-scoping for free. |
| Per-session state registry | `SessionRegistry` (`crates/core/src/daemon/session.rs`) | Collectors follow the triggers/filters/bookmarks pattern for lifecycle and persistence. |
| Per-trigger debounce | `post_window_remaining` (`crates/core/src/daemon/session.rs`) | Landed in 0.3.0. Threshold triggers (§8) reuse it rather than inventing cooldown. |
| File export | `logs.export` | Establishes the file-writing pattern `collectors.document` (§9) follows — though it produces a data file, where §9 produces a document meant to be read. |
| Span ring buffer | `SpanStore`, default `span_buffer_size` **10 000** (`crates/core/src/daemon/persistence.rs`) | The reason a query-only design is insufficient: a suite-length run overflows this and eviction is silent. |

### 1.1 Two existing defects this work touches

- **`traces.slow` `group_by="name"` aggregates a biased sample.**
  `SpanStore::slow_spans` filters to `duration_ms >= min_duration_ms`, sorts
  descending, and **truncates to `count`** — and only then does the handler group.
  With the defaults, the reported `avg_ms`/`p95_ms` are computed over the 20 slowest
  spans that already cleared 100 ms. That "p95" is a p95 of the tail. Once the
  projection core (§5) exists, `traces.slow`'s grouped arm becomes a thin wrapper
  over it and the bias disappears. **Fixed in-scope.**
- **The trigger loop re-parses each trigger's filter string per span**
  (`parse_filter(&trigger.filter_string)` inside the per-span loop). Acceptable at
  trigger volumes, not at collector volumes. Collectors store a pre-parsed
  `ParsedFilter` at arm time (§4.2). The pre-existing trigger re-parse is **not**
  fixed here — noted as a separate follow-up.

---

## 2. Scope

**In scope**

1. Collector object: create/arm, list, read, snapshot, reset, remove — per session.
2. Two-tier retention: an always-exact tier (scalars + duration histogram) and a
   capped columnar sample tier.
3. Retention levels (`scalar` / `timing` / `tree`) chosen at definition time.
4. Optional `group_keys` — span attributes retained as grouping dimensions.
5. Read-time projections: sum/count/avg/min/max, percentiles (histogram-estimated
   over the whole run, and sample-exact where the sample is complete), self-time,
   wall-clock union, nested-match detection, grouping by name / trace / attribute /
   **call path**, and warm-up exclusion.
6. **Snapshot history** — snapshot-and-zero between runs, with a declared snapshot
   policy, retained history, and merging (§6).
7. **Descriptions** on collectors and on each snapshot, echoed in every result.
8. `traces.profile` — the same projection over the ring buffer, no collector needed.
9. `collectors.diff` — deltas at every percentile, with mismatch refusals; accepts
   snapshots on either side.
10. `collectors.document` — a long-lived record of a collector and its full snapshot
    history, designed backwards from what a reader months later needs: triage
    front-matter, the recorded question and finding, the computed comparison, a
    noise floor, and a stated set of limitations (§9). Folded-stack output for
    speedscope / flamegraph.pl is one of its formats.
11. Threshold triggers on collectors (phase 5).
12. Fixing the `traces.slow` grouping bias (§1.1).

**Non-goals**

- Log-derived durations (aggregating a numeric GELF field for non-OTLP apps).
  Broadens reach, but it is a parallel ingest path and nothing in the driving case
  needs it. Deferred (§13).
- **Cross-collector** baselines — a shared store you can compare against across
  projects or machines. Snapshot history (§6) absorbs the in-collector case, which
  is what the driving workflow needs; a global baseline store is a separate surface
  with its own naming and retention questions. Deferred (§13).
- **Re-importing a document.** There is no `collectors.import` and no frozen-collector
  concept. A document's consumer is a reader — an AI assistant or a person — doing the
  comparison themselves (§9.1). Dropping the round trip removes an RPC, a lifecycle
  state, and an entire format-compatibility obligation.
- Multi-key `group_by`. Single dimension in v1.
- Reservoir sampling. Explicitly rejected — see §3.4.
- Any change to how spans are received or stored.

---

## 3. Data model

### 3.1 Two retention tiers

**Exact tier** — `{count, sum_ms, min_ms, max_ms}` **plus a duration histogram**, for
the collector as a whole and the same again per span name. Memory is O(distinct span
names), which is small and bounded by real span vocabularies. **Never capped.** The
four scalars are never approximate; the histogram carries a bounded relative error by
construction (§3.5). This tier carries the headline A/B number, so the figure quoted
in "cache on vs cache off" stays exact regardless of run length.

**Sample tier** — one record per matched span, stored **columnar** (struct-of-arrays,
one `Vec` per column). Columnar rather than array-of-structs for two reasons: no
alignment padding around the `u128` trace id, and levels fall out naturally as
"which columns are allocated" instead of a variant layout.

### 3.2 Retention levels

Ordered, each a superset of the previous. Chosen per collector at definition time.

| Level | Columns added | Unlocks | Bytes/match |
|---|---|---|---|
| `scalar` | *(none — exact tier only)* | count, sum, avg, min, max, **estimated percentiles** (§3.5) — total and per span name | 0 |
| `timing` | `start_ns i64`, `end_ns i64`, `name_id u32`, `flags u8` | sample-exact percentiles, wall-clock union, warm-up exclusion | 21 |
| `tree` **(default)** | + `span_id u64`, `parent_span_id u64`, `trace_id u128` | self-time, nested-match detection, per-trace rollups, call-path aggregation, folded export | 53 |

Plus **4 bytes per match per `group_keys` entry** at `timing` and above (one interned
`u32` column each). At `scalar` there is no sample tier and therefore no column —
group keys instead widen the **exact tier's key**, which becomes
`(span name × group-key values)`. Bounded by the §3.3 cardinality cap, so still
O(vocabulary) rather than O(matches).

Histograms are per-collector / per-name / per-group, **not** per-match, so they do
not appear in this table — see §3.5 for their cost.

> Byte figures are **derived from field sizes, not measured.** They must be
> re-derived against `size_of` in the phase-1 tests (§12, V1), not trusted from
> this table.

`flags` packs span status (2 bits) and kind (3 bits).

An ordered level was chosen over per-field opt-in deliberately: per-field flags
produce a combinatorial matrix and force every projection to carry its own
"field absent" path, for flexibility nobody exercises. With an ordered level, each
metric declares one minimum (§5.2) and the error is a single clear sentence.

### 3.3 `group_keys`

A list of span **attribute names** whose values are interned per collector.
Orthogonal to level: at `timing` and above they are retained as a `u32` column per
match, which is what enables per-group *percentiles*; at `scalar` they widen the
exact tier's key instead (§3.2), giving exact per-dimension sums at near-zero memory.

This exists because it changes how the A/B is run. If the system under test emits
its kill-switch state as a span attribute, **one collector with
`group_keys: ["cache.enabled"]` captures both arms**, and the comparison becomes a
projection rather than a reset-between-runs ritual. The arm label travels with the
data instead of living in the operator's head — which is precisely the failure mode
the "arms must differ by exactly the capability under test" rule exists to prevent.

**Cardinality hazard.** A key like `request.id` would grow the intern table without
bound. Each key gets a cardinality cap (default 1024 distinct values). Past the cap,
further values fold into a literal `__overflow__` bucket and the result carries
`cardinality_capped: true` naming the offending key. Bounded, and never silent.

### 3.4 Cap behaviour, and why not reservoir sampling

Per-collector budget `max_sample_bytes`, default **64 MiB** (≈1.27 M matches at
`tree`, derived). Daemon-wide `max_total_sample_bytes`, default **256 MiB**;
`collectors.add` fails loudly with both numbers if arming would exceed it.

On reaching the per-collector budget the sample tier **stops retaining** — prefix
truncation. The exact tier keeps running, so `count`/`sum`/`min`/`max` remain exact
for the entire run. The result then carries `sampled.complete = false` alongside
`sampled.sample_count`.

**Reservoir sampling is rejected.** A uniform random sample gives unbiased
percentiles, but it breaks the two projections that need *completeness*: self-time
needs a span's children present, and wall-clock union needs every interval. Prefix
truncation keeps all projections valid *for the retained prefix*, which is a
defensible statement; a reservoir would make self-time quietly wrong.

Prefix truncation has its own bias — the retained prefix is the **cold** part of the
run, exactly the wrong sample for a cache benchmark. That is why `complete=false` is
structural rather than a footnote, and why `collectors.diff` refuses to compare an
incomplete side by default (§5.6).

The histogram (§3.5) is what keeps that bias from reaching the numbers most likely to
be quoted: it is fed at ingest, so percentiles cover **every** match no matter how
early the sample tier stopped retaining. Truncation degrades self-time, wall-clock
union, and paths — not the distribution.

### 3.5 Duration histogram

A log-linear histogram in the exact tier, updated once per match at ingest: O(1), no
allocation in the steady state, **~4 KB** each (derived from bucket count, not
measured). One per collector, one per span name, and one per group up to
`max_group_histograms` (default 64) — past that a group keeps its exact scalars only
and the result says so, bounding worst-case histogram memory.

**Why at ingest rather than at read or snapshot time.** A histogram built later from
the sample tier would inherit that tier's prefix truncation, and a long run that blew
the budget would be laundered into a clean-looking snapshot. Fed at ingest, the
histogram is complete by construction and independent of the sample cap. This is the
single reason percentiles survive both truncation *and* snapshotting.

Percentiles read from it are reported as **`estimated`**, carrying their relative
error bound, and are kept structurally distinct from the sample tier's `sampled`
percentiles, which are exact for whatever the sample retained (§5.1).

Histograms are **additive** — merging two is exact, not an approximation of a merge.
That is what makes snapshot merging (§6.5) possible at all.

Prior art, not invention: HDR Histogram and DDSketch both solve this. Port a proven
bucket layout rather than deriving the boundary math fresh — the standing rule to
reconcile a new mechanism against its reference applies squarely here, and bucket
math is exactly the kind of thing that reads correct and is off by one bucket.

---

## 4. Ingest path

### 4.1 Hook

A third step in `process_span_for_domain`, after storing and after trigger
evaluation:

```
1. store.insert(span)                        (existing)
2. evaluate span triggers for this domain    (existing)
3. evaluate collectors for this domain       (NEW)
```

### 4.2 Cost discipline

The ingest path runs at suite-length span rates, so:

- Filters are **pre-parsed** at `collectors.add` and stored as `ParsedFilter`.
  No `parse_filter` on the hot path.
- Matching allocates nothing. `matches_span` is already allocation-free for the
  selectors involved.
- Appending to a columnar buffer is amortised O(1); columns reserve in blocks.
- Name/attribute interning is a hash lookup returning an existing `u32` in the
  steady state.

**Hazard — collector lookup must be domain-keyed, not session-derived.** The
existing trigger path finds work via `sessions.active_session_ids_for_domain(domain)`.
If collectors are discovered the same way, a session that rebinds its domain
mid-run would silently stop feeding its collector. A collector is **pinned to the
domain it was created in** and keeps collecting from that domain regardless of what
its owning session later binds. This requires a domain-keyed collector registry, not
a session-domain-derived scan. Adversarial test A5 covers it.

**The existing span path is allocation-heavy — do not copy its shape.** Verified by
reading the calls, not assumed. Per span, `process_span_for_domain` performs:

- `active_session_ids_for_domain` — sessions read lock, full session scan, one cloned
  `SessionId` per match into a fresh `Vec`;
- then per session, `list_triggers` — the sessions read lock **again**, then a
  triggers read lock, then `to_info()` per trigger; `TriggerInfo` carries
  `filter: String` and `description: Option<String>`, so that is a heap allocation
  per trigger per span;
- then `parse_filter(&trigger.filter_string)` per trigger.

Three-plus lock acquisitions, N+M allocations, and a full filter parse — per span.

Collectors repeat none of it: a domain-keyed registry read under a single lock,
pre-parsed filters, and borrowed access to collector state with no `*Info`
materialisation on the ingest path.

This also fixes the baseline for A14 (§12). logmon's span ingest is already heavier
than it looks, so collector overhead must be measured against the **current** path,
not against zero. The pre-existing cost is **not** fixed in this work — it is filed
as a separate follow-up.

### 4.3 Ordering and clocks

Because every metric is a read-time projection, **span arrival order does not
affect any result.** Children may arrive before or after parents; traces may
interleave. Adversarial test A3 pins this.

`start_ns`/`end_ns` come from the span's own timestamps — the **producer's** clock,
not broker receipt time. Across processes with skewed clocks, wall-clock union and
call-path timing inherit that skew. Stated in the tool docs; not corrected here.

The collector's own window (`armed_at` … `read_at`) is broker clock and is reported
separately, so the two are never conflated.

---

## 5. Read-time projections

One projection module consumes an iterator of sample records and produces a
`ProfileResult`. It is fed either by a collector's sample tier or by a scan of the
ring buffer (`traces.profile`) — the two entry points differ only in the source.

### 5.1 Result shape

The exact/sampled distinction is **structural**, not a footnote:

```jsonc
{
  "collector": "cache-ab",
  "description": "Store suite — schema-cache A/B",   // §6.6, echoed everywhere
  "filter": "sv=store_server",
  "level": "tree",
  "matched": 48213,                  // exact tier — every match, always
  "window": { "armed_at": "...", "read_at": "...", "wall_ms": 812004.0 },

  "exact": {                         // null iff warm-up exclusion is active (§5.5)
    "count": 48213, "sum_ms": 1234567.8,
    "avg_ms": 25.6, "min_ms": 0.1, "max_ms": 4310.2
  },

  "estimated": {                     // histogram — covers EVERY match, always present
    "error_pct": 1.0,
    "p50_ms": 12.0, "p80_ms": 41.0, "p95_ms": 181.0, "p99_ms": 900.0
  },

  "sampled": {                       // sample tier — exact for what it retained
    "complete": true,                // false ⇒ prefix-truncated at the budget
    "sample_count": 48213,
    "p50_ms": 12.0, "p80_ms": 41.2, "p95_ms": 180.4, "p99_ms": 902.1,
    "self_time_ms": 998001.2,        // tree
    "wall_union_ms": 310221.0,       // timing
    "nested_matches": 0,             // tree
    "error_count": 12
  },

  "groups": [ { "key": "...", "exact": {…}, "estimated": {…}, "sampled": {…} } ]
}
```

Three categories, and the distinction is structural rather than a footnote, so a
degraded number cannot be quoted as an exact one:

- **`exact`** — the four scalars. Never approximate, never truncated.
- **`estimated`** — histogram percentiles over the *whole run*, ±`error_pct`.
- **`sampled`** — exact for the records the sample tier retained, and everything that
  needs completeness (self-time, wall-union, paths). Absent at `scalar`.

**Which to quote.** When `sampled.complete` is `true` the two percentile sets agree
within `error_pct`, and `sampled` is the sharper number. When it is `false`,
`estimated` is the one that describes the run and `sampled` describes only its cold
prefix — the case where reading the wrong field is most tempting and most wrong.

Percentile list is a parameter; default `[50, 80, 95, 99]`.

### 5.2 Metric catalogue and minimum level

| Metric | Min level | Notes |
|---|---|---|
| `count`, `sum_ms`, `avg_ms`, `min_ms`, `max_ms` | `scalar` | Exact tier. Never degraded. |
| `estimated.p*_ms` | `scalar` | Histogram (§3.5). Whole run always, ±`error_pct`. |
| `sampled.p*_ms` | `timing` | Nearest-rank over the retained sample — exact for that sample, which is the whole population unless `sampled.complete` is `false`. |
| `wall_union_ms` | `timing` | Merged `[start, end)` coverage. |
| `error_count` | `timing` | From `flags`. |
| `self_time_ms` | `tree` | Duration minus **retained** children. |
| `nested_matches` | `tree` | Matched spans whose parent is also matched. |
| `group_by: "trace"` | `tree` | |
| `group_by: "path"` | `tree` | §5.4 |

Requesting a metric above the collector's level returns a **loud error naming the
level required** — never a zero, which would read like a real measurement.

### 5.3 Grouping

`group_by` accepts `"name"`, `"trace"`, `"path"`, or `"attr:<key>"` where `<key>`
must be one of the collector's `group_keys`. An unknown key errors with the
available keys listed. Single dimension in v1.

### 5.4 Call-path aggregation — the flat profile

`group_by: "path"` reconstructs each sample's ancestor chain **within the matched
set** and aggregates self-time by path rather than by leaf name. The difference is
"`schema.resolve` costs 40 s" versus "40 s, 90 % of it under `reconcile`" — only the
second identifies what to change.

Algorithm: build `span_id → row` from the retained column, walk `parent_span_id`
upward, join names root→leaf.

Three safety properties:

- **Depth cap** (64) and a visited-set check. Malformed or adversarial data must not
  produce an unbounded or cyclic walk. Adversarial test A2.
- **Incompleteness is marked, not hidden.** If a walk stops at a span whose
  `parent_span_id` is non-zero but unretained, the path is prefixed `[?]` and the
  group carries `path_incomplete: true`. It is never silently rooted at the wrong node.
- **Broad filters are the idiom.** Paths only resolve where ancestors are also
  matched, so `sn=schema.resolve` yields leaf-only paths by construction. The
  profiling idiom is a broad collector filter (`sv=store_server`) plus read-time
  narrowing. This belongs in the skill/tool docs, not in code.

### 5.5 Warm-up exclusion

`skip_warmup_ms: N` drops samples whose `start_ns` falls in the first N ms of the
window. Cold caches, JIT, and connection-pool fill are the largest source of invalid
cache benchmarks, and the bias is **asymmetric** — warm-up penalises the cache-on
arm hardest, biasing against the very effect being measured.

Because the exact tier is unwindowed, when `skip_warmup_ms > 0` the response sets
`"exact": null` and `"exact_unavailable_reason": "warmup_excluded"`, and all totals
come from the sample tier. The same applies per group — `groups[].exact` is null
under warm-up exclusion. Unambiguous by construction: there is no path on which a
warm-up-excluded run reports an unwindowed sum in an `exact` block.

### 5.6 `collectors.diff`

`collectors.diff(a, b, group_by?, percentiles?)` — a pure read-time projection over
two existing results. No new state.

Reports absolute and relative deltas for `count`, `sum_ms`, and **each percentile**,
overall and per group. Percentile-wise deltas are the point: a mean-only diff is how
"8 % faster" gets reported when 8 % is noise, and if p50 moves while p95 does not,
the change is helping the common path and not the tail — a materially different
conclusion, invisible without the shape.

**Two refusals, both provable from recorded facts rather than inferred** (a blocking
check must flag only what cannot be correct):

1. **Mismatched arms.** Different `filter` or different `level` between `a` and `b`
   → refuse. Comparing arms that differ by more than the variable under test is the
   defect the whole feature exists to avoid.
2. **Incomplete side.** Either side with `sampled.complete == false` → refuse unless
   `allow_incomplete: true`, because a prefix-truncated arm is cold-biased and
   comparing it to a full run manufactures a wrong answer.

Both errors state the mismatch concretely.

Either side may be a live collector or a snapshot (`collector@label`, §6.2), so
snapshot↔snapshot, snapshot↔live, and collector↔collector are one call. When the two
sides carry different representations, the diff compares at the **weaker** of the two
and says which — comparing an `estimated` percentile against a `sampled` one without
saying so would be the same apples-to-oranges defect the refusals above exist to
prevent.

---

## 6. Snapshots and history

### 6.1 Why

The driving workflow is inherently multi-run: define a collector, run the suite,
snapshot, change one variable, run again, snapshot. Without in-collector history, the
numbers have to be copied out by hand between runs — which is precisely the
error-prone, context-losing step this feature exists to remove.

Snapshots are also what make a run **durable**. The live sample tier cannot survive a
daemon restart (§10), but a snapshot is small enough to persist, so a completed run
stops being at risk the moment it is snapshotted.

### 6.2 Operations

| Call | Behaviour |
|---|---|
| `collectors.snapshot(name, label?, description?, reset=true)` | Record a snapshot, return it, and by default zero the live tiers. |
| `collectors.reset(name)` | **Discard** without recording. The botched-run path — a mis-configured run must not pollute history. |
| `collectors.history(name, limit?, merge?)` | Snapshot descriptors; `merge` combines them (§6.5). |
| `collectors.get(name, snapshot=label)` | Read one snapshot. |
| `collectors.diff(a, b)` | Either side may be `collector` or `collector@label`. |

Unlabelled snapshots auto-label `snapshot-<n>`. Every snapshot records `taken_at` and
its window. `max_snapshots` (default 50) evicts FIFO and reports `snapshots_evicted`,
so a long-running collector loses history visibly rather than silently.

Keeping `reset` distinct from `snapshot` is deliberate: one of them is how you throw
away a run you know is invalid, and collapsing them would mean either polluting
history or having no way to discard.

### 6.3 What a snapshot contains — declared at definition time

The sample tier cannot be copied N times, so a snapshot keeps a *projection* of it,
and the collector declares which projections when it is defined:

```jsonc
"snapshot": {
  "per_name": true,          // exact tier — cheap
  "per_group": true,         // exact tier
  "projections": ["self_time", "wall_union", "top_paths:100"],
  "raw_sample_bytes": 0      // 0 = off; >0 keeps a prefix for full later recomputation
}
```

**Always present regardless of policy**, because they come from the exact tier: the
total and per-name scalars, and the histograms. So **percentiles are available on
every snapshot, for the whole run**, at the stated error bound — the policy governs
only the sample-derived extras.

The default is right for the driving case with no configuration written:
`{per_name: true, per_group: true, projections: ["self_time", "wall_union",
"top_paths:100"], raw_sample_bytes: 0}`.

`raw_sample_bytes > 0` keeps a **prefix** of the sample records, with the same
semantics and the same `complete` flag as the live cap (§3.4). It is not downsampled:
uniform downsampling would keep percentiles honest but silently break self-time and
path aggregation, which need complete parent–child sets. Same reasoning as the
reservoir rejection, same answer.

### 6.4 Four disciplines

1. **Validated at definition time.** `top_paths` on a `timing` collector errors at
   `collectors.add` — not after a 40-minute run has already been measured.
2. **Size is quoted up front.** `collectors.add` returns the derived per-snapshot
   size and the total at `max_snapshots`, so the memory consequence is visible before
   it is committed to rather than discovered later.
3. **Each snapshot echoes its own policy.** Policies are editable via
   `collectors.edit`, and editing must not retroactively rewrite history — so an old
   snapshot has to be able to explain its own gaps. Asking a snapshot for something
   its policy excluded returns a loud error naming the policy, mirroring the level
   errors of §5.2.
4. **Completeness travels with the data.** A snapshot records the `complete` flag of
   the sample tier its sample-derived projections were computed from. Histogram
   percentiles remain whole-run regardless; self-time, wall-union, and paths inherit
   the truncation and say so.

### 6.5 Merging

Exact scalars and histograms are both additive, so `collectors.history(name,
merge: [labels])` combines runs into a single distribution — the direct way to beat
down run-to-run noise across repetitions instead of arguing about which single run
was representative.

Sample-derived projections do **not** merge: self-time and paths need the underlying
records, and averaging two path tables is not a path table. A merged result omits
them and states that it did, rather than presenting a plausible-looking blend.

### 6.6 Descriptions

`description` on the collector (set at `add`, editable); `label` + `description` on
each snapshot, plus free-form `meta` key/values carrying the provenance logmon cannot
infer — commit, build profile, varied configuration (§9.3). All are echoed in every
read, diff, history entry, and document.

This is the "a load-bearing number carries the context it was measured in" rule
enforced by the data structure rather than by memory: there is no path on which a
number leaves the broker without the description of the collector — and, for a
snapshot, the run — that produced it.

---

## 7. Contract surface

Following the existing `noun.verb` convention (`triggers.add`, `traces.slow`).

| JSON-RPC | MCP tool | Purpose |
|---|---|---|
| `collectors.add` | `add_collector` | Define and **arm immediately**. Params: `name`, `filter`, `level`, `group_keys`, `max_sample_bytes`, `description`, `snapshot` (§6), `threshold` (§8). |
| `collectors.list` | `get_collectors` | Definitions + live counters + memory used. |
| `collectors.get` | `get_collector` | Read a `ProfileResult`. Projection params: `group_by`, `percentiles`, `skip_warmup_ms`, `snapshot`. Non-destructive. |
| `collectors.snapshot` | `snapshot_collector` | Record a labelled snapshot, return it, zero the live tiers (§6.2). |
| `collectors.history` | `get_collector_history` | List snapshots; `merge` combines them (§6.5). |
| `collectors.edit` | `edit_collector` | Change `description`, `snapshot` policy, `threshold`. Not the filter or level — see below. |
| `collectors.reset` | `reset_collector` | **Discard** without recording. Returns what it cleared. |
| `collectors.remove` | `remove_collector` | |
| `collectors.diff` | `diff_collectors` | §5.6. Either side may be `collector@label`. |
| `collectors.document` | `document_collector` | §9. Params: `names`, `format`, optional `path` (omit to get the document back in the response), and the operator-supplied `question` / `finding`. Regenerable and lossless — re-render once the finding is known. Accepts several collectors in one call. |
| `traces.profile` | `profile_spans` | Same projection over the ring buffer. Accepts bookmark windows (`b>=`/`b<=`) — already supported on span filters. |

`collectors.snapshot` recording and zeroing in one call makes run→record→zero→run
atomic: there is no window in which the record has happened, the zero has not, and a
straggling span lands in the wrong arm. `collectors.reset` gives the same atomicity
for the discard case.

**`collectors.edit` cannot change `filter` or `level`.** Both would silently
invalidate every snapshot already in the collector's history against a definition
that no longer describes them — and §5.6 refuses cross-filter diffs precisely because
that comparison is meaningless. Changing either means a new collector, which the error
says.

Wire discipline: new types are additive; `cargo xtask verify-schema` regenerates
`protocol-v1.schema.json` and the result is committed in the same change.

CLI mirrors the MCP surface 1:1, per the project's standing rule
(`logmon-mcp collectors add …`, `logmon-mcp collectors get <name> --json`).

---

## 8. Threshold triggers (phase 5)

A collector may carry `threshold: { metric, group?, op, value, window_ms }` —
e.g. p95 of `sn=db.query` over a rolling 60 s window exceeding 200 ms. On crossing,
it emits the same notification shape span triggers already emit.

Cheap because both halves exist: span triggers already fire at ingest, and
per-trigger debounce (`post_window_remaining`) already solves cooldown. Neither is
reinvented.

This turns logmon from pull to push for performance: a guard armed before a
refactor reports a regression instead of waiting to be asked.

**Design hazard.** Evaluate against a **rolling window**, not the since-arm
aggregate. A since-arm p95 becomes progressively unmovable as samples accumulate,
so a guard built on it goes quietly deaf exactly when a run gets long enough to
matter. Rolling-window evaluation is the requirement, not an optimisation.

---

## 9. Documenting a collector

`collectors.document(names, format, path)` — MCP `document_collector`.

### 9.1 What it is for — two moments, one artefact

A document serves two readers who turn out to want the same content for different
reasons:

- **Now.** The run has just finished. The document is the *synthesis* — it assembles
  what would otherwise be a dozen scattered `collectors.get` calls into one picture,
  and its job is to show what moved and what to do next.
- **Months later.** A new optimisation idea comes up and the question is what this
  cost before and what has changed since. Now the job is triage and trust.

Designing for only the second produces an artefact nobody opens until it is too late
to fix the measurement; designing for only the first produces one that is
uninterpretable once the session's context is gone. Both are cheap to serve together
because the same content answers both — the differences are in *ordering* (§9.4) and
in whether a limitation reads as a caveat or an instruction (§9.6).

Three requirements follow that a plain data dump fails:

1. **It documents the snapshots, not just the live state.** A collector's history *is*
   the record; documenting only the current tiers would capture the least interesting
   moment.
2. **It is self-describing.** The later reader has no logmon, no spec, and no memory of
   the session. Everything needed to interpret the numbers travels in the file.
3. **It is not re-imported.** There is deliberately no `collectors.import` and no
   frozen-collector concept. Comparison happens in the reader, not in the broker —
   which is why the artefact is optimised for being *read*, not for round-tripping.

**A document is a render, not a commit.** `path` is optional: omit it and the document
is returned in the response, which is the present-tense path — writing to disk and
reading it back is two round trips and a filesystem dependency for something that can
be one call. Because the collector still holds every snapshot, regenerating is free
and lossless.

That property is load-bearing rather than incidental: **`finding` normally arrives
after the first read.** You render the document in order to *form* the conclusion,
then re-render with the conclusion attached. A design that demanded the finding up
front would be demanding the answer before the question.

### 9.1.1 `get` versus `document`

Two reads of the same collector, and the difference should never be guessed at:

- **`collectors.get`** answers a question you already have. It is parameterised —
  `group_by`, `percentiles`, `skip_warmup_ms` — and returns one projection.
- **`collectors.document`** tells you which questions to ask. It is a fixed,
  comprehensive synthesis: comparison, noise floor, vocabulary, limitations, ranked
  movers.

The document is where you start; `get` is where you follow up. Neither replaces the
other, and if a future change makes one a strict subset of the other, that is the
signal that one of them should be removed.

The name is `document`, not `export`: `logs.export` produces a data file, and this
produces an artefact meant to be read. It is deliberately not `insights` — logmon
computes comparisons, spreads, vocabularies, and rankings; the insight is the reader's.
A file promising insight and containing only data is worse than one that promised
data.

### 9.2 Derived from the reader's five questions

The contents below are not a wish list; each item earns its place by answering one of
the five questions a consumer actually asks. Anything answering none of them does not
belong in the document.

| The reader asks | When | Which means the document must |
|---|---|---|
| Is this about my thing? | Later | Be triageable **without being read** — across a directory of thirty files (§9.3) |
| Can I trust it, and is it comparable to what I am about to run? | Later | Carry its environment and its own limits (§9.3, §9.6) |
| What did it conclude? | Both | Record the question and the finding, not only the data (§9.3) |
| Was the effect real or noise? | Both | Quantify run-to-run spread when it can (§9.5) |
| **What should I do next?** | **Now** | Rank what moved, and pair every limitation with its remedy (§9.4, §9.6) |

The last row is what the present-tense moment adds, and it is why §9.6 is written as
instructions rather than disclaimers.

### 9.3 Triage front-matter

The document opens with a YAML front-matter block — small, fixed-schema, and
`grep`-able, so thirty documents can be scanned without opening any of them:

```yaml
format_version: 1
collector: cache-ab
question: "Does the schema cache reduce total time in store_server spans?"
finding: "Yes — total work −38%; p95 unchanged, so it helps the common path only."
measured_at: 2026-07-29T14:02:11Z
filter: "sv=store_server"
filter_intent: "all server-side work, broad on purpose so call paths resolve"
services: [store_server]
git_sha: a08c119
build_profile: release
parallelism: 16
host: imac-studio
runs: 6
trustworthy: true          # false when any arm was truncated or single-run
headline: "sum_ms 1234567 → 765432 (−38.0%)"
```

Three of these fields carry most of the triage weight and none of them were in the
earlier draft:

- **`question` / `finding`.** Data alone is a log; a recorded conclusion is memory.
  These are supplied by the operator at document time — logmon cannot infer either,
  and a document that omits them makes a future reader re-derive a judgement that was
  already made once.
- **`filter_intent`.** A filter string is precise and nearly useless as a relevance
  signal months on. One plain sentence about what was deliberately in and out of scope
  is what actually decides "is this about my thing".
- **`build_profile`.** The single most damaging silent mismatch available: comparing a
  debug-build measurement against a release run is worse than having no prior data,
  because it looks like data. It sits in front-matter rather than buried in `meta`
  for that reason.

Recommended filename `YYYY-MM-DD-<collector>-<slug>.md`, so `ls` alone is informative.

### 9.4 Body, in reading order

**Order is part of the design, not formatting.** The body runs most-decision-relevant
first and reference material last — the reverse of how a spec would naturally be
written. The present-tense reader wants the conclusion and the next action on the
first screen, and the later reader has already triaged on front-matter and wants
exactly the same thing.

1. **What moved.** The computed comparison across arms, at every percentile (§5.6),
   already worked out and **ranked by contribution**. A reader should not be
   subtracting forty numbers by hand to recover a conclusion that was obvious at the
   time, and ranking is what turns a table into a starting point.
2. **What to do next.** The actionable limitations (§9.6) and the noise-floor verdict
   (§9.5). This is the present-tense payload.
3. **Per-snapshot detail** — label, description, `taken_at`, window, `meta`, and the
   scalar/percentile tables. Units in every column header; `_ms` suffixes retained in
   field names.
4. **The full per-span-name vocabulary**, not the top rows. A reader comparing two
   documents months apart needs to see that names appeared or disappeared — the signal
   that the code moved underneath and the comparison is no longer apples to apples.
   logmon cannot compute this across two files it will never read again, so its job is
   to ensure the file contains what the reader needs to spot it.
5. **Definition and how to read the numbers** — filter, level, `group_keys`, snapshot
   policy, description, `meta`, and the `exact` / `estimated` / `sampled` distinction
   with what `complete: false` means. Last because it is reference material: needed to
   interpret, never the reason you opened the document. A data format explaining its
   own conventions in prose is unusual and correct here — the three-category structure
   exists to stop a degraded number being quoted as an exact one, and that protection
   is worth nothing to a reader who no longer remembers the convention.

`format: "json"` writes the structured data alone. `format: "folded"` emits
`nameA;nameB;nameC <self_time_us>` per snapshot for speedscope and flamegraph.pl;
requires `tree`. Several collectors may be named in one call, so documenting a whole
session is one operation.

### 9.5 Noise floor

When an arm has two or more snapshots, the document reports the run-to-run spread —
min, max, and coefficient of variation across repeats of the *same* configuration.

An 8 % improvement above a 2 % noise floor is a finding; the same 8 % above a 15 %
floor is nothing. This is the only thing in the document that separates the two, and
it is **strictly unrecoverable** if not captured at measurement time — a document can
be re-read, a suite from six months ago cannot be re-run.

It reads differently at the two moments, which is the clearest illustration of why
§9.6 is phrased as instructions. Later, "floor unknown, single run" is a reason to
discount the result. **Now, it is a reason to run it again** — and acting on it costs
one more run, where discovering it later costs the whole measurement.

When an arm has a single run the document states the floor is unknown rather than
omitting the field, because an absent floor reads as a stable one.

### 9.6 What the document must admit

A short, always-present section stating what the measurement *cannot* tell you — and,
because the reader is often standing at the machine that produced it, **what to do
about each one**:

| Limitation | Remedy, stated in the document |
|---|---|
| An arm was truncated (`complete: false`) | Raise `max_sample_bytes` or narrow the filter, and re-run that arm — its self-time and paths describe only the cold prefix |
| Percentiles are `estimated` | Bound is ±`error_pct`; treat differences inside it as no difference |
| Single-run arm | Floor unknown — repeat the arm before concluding (§9.5) |
| Warm-up excluded | By how much, and that `exact` is therefore null (§5.5) |
| Raw sum under N-way parallelism | This is total work, not wall clock; do not read it as elapsed time |
| Spans outside the filter | Absent entirely — the document cannot tell you what it never saw |
| Several services present | Cross-process clock skew reaches wall-union and path timings (§4.3) |

This table is the difference between the two moments. Months later these are caveats
governing how much to trust a number. In the present they are instructions, and each
one is cheap to act on now and impossible to act on later. Every entry is knowable at
document time, and a reader forced to *infer* them will get some wrong.

### 9.7 Sizing: document small, data beside it

The readable document stays small — target tens of KB — and bulk data goes in a
sidecar named in the front-matter: raw histogram buckets, per-trace tables, and any
`raw_sample_bytes` prefix.

**This reverses an earlier draft** that embedded the complete JSON in the document to
keep it self-contained. From the reading seat that is backwards: the consumer loads
this file into a limited context, and a multi-megabyte blob means either it cannot be
opened or it consumes the budget the analysis needed. Self-containment is worth less
than being readable at all. The document remains useful standalone; the sidecar is
fetched only when the analysis needs the raw distribution.

### 9.8 Two details that only matter at this timescale

- **Histogram bucket layout travels with the buckets**, never assumed from the reading
  side's defaults. Otherwise a later change to the default layout silently shifts every
  percentile in every older document.
- **`format_version` is stamped.** Informational rather than enforced — nothing
  imports, so there is no compatibility gate to fail — but a reader must be able to
  tell which convention it is looking at.

Because nothing re-imports, there is no round-trip correctness obligation and no
format compatibility contract to break. This surface is ordinary T2: the failure mode
is a document that *reads* misleadingly, which is a writing problem rather than a
data-integrity one.

---

## 10. Lifecycle, persistence, restart

- **Definitions persist** for named sessions, exactly as triggers/filters/bookmarks
  do. Anonymous-session collectors die with the session.
- **Snapshots persist** too, for named sessions. They are small (§6.3) and they are
  the record of a completed run, which is the thing least acceptable to lose. This is
  what makes snapshotting after each run the durable workflow: an A/B spanning a
  restart keeps every run that was snapshotted.
- **Live accumulated data does not survive a daemon restart** — the sample tier is up
  to the memory budget and is not written to disk.
- On restore, a collector comes back **armed but zeroed**, carrying
  `zeroed_by: "daemon_restart"` and `zeroed_at` until the next explicit reset or
  snapshot. A restart mid-run is therefore *visible in the result*, not a silently
  partial run that reads like a complete one. Its persisted history is unaffected.
- A collector is pinned to its creation domain (§4.2).
- Duplicate collector name within a session → error, mirroring `rename_session`'s
  refusal to let two live identities collide.

---

## 11. Error handling

Every one of these is a loud, specific error — never a zero, an empty result, or a
silent fallback:

| Condition | Response |
|---|---|
| Metric above the collector's level | Error naming the level required |
| `group_by: "attr:x"` with `x` not in `group_keys` | Error listing available keys |
| Log-selector filter given to a collector or `traces.profile` | Error (reuses `is_span_filter`) |
| Cursor qualifier (`c>=`) in a collector filter | Rejected, as `traces.slow` already does |
| Arming would exceed the daemon budget | Error stating requested and remaining bytes |
| Unknown / duplicate collector name | Error |
| `diff` on mismatched filter or level | Refused, naming the mismatch (§5.6) |
| `diff` with an incomplete side | Refused unless `allow_incomplete` (§5.6) |
| Group-key cardinality cap hit | `__overflow__` bucket + `cardinality_capped: true` |
| Sample budget hit | `sampled.complete = false`; exact tier and histogram unaffected |
| Zero matches | Well-formed empty result — no sentinel, no division by zero |
| Snapshot policy names a projection above the level | Error at `collectors.add`, not at snapshot time (§6.4) |
| Reading a snapshot for something its policy excluded | Error naming the excluding policy (§6.4) |
| Unknown snapshot label | Error listing available labels |
| `collectors.edit` changing `filter` or `level` | Refused — it would invalidate existing history (§7) |
| `max_snapshots` reached | Oldest evicted FIFO; `snapshots_evicted` reported |
| Merging snapshots with sample-derived projections requested | Those fields omitted, with a stated reason (§6.5) |
| Group histogram cap reached | Group keeps exact scalars only; result says so (§3.5) |
| `document` with `format: "folded"` on a non-`tree` collector | Error naming the level required |
| `document` of a collector with no snapshots | Valid — documents the definition and live tiers, and states that history is empty |
| `document` without `question` / `finding` | Valid, but front-matter records them as absent and `trustworthy` is not asserted — a silently missing conclusion must not read like a considered one |
| Any arm truncated, or every arm single-run | `trustworthy: false` in front-matter, with the reason (§9.3) |

---

## 12. Test list (verification + adversarial)

**Verification**

- **V1** Each level allocates exactly its columns; per-match memory matches the §3.2
  table re-derived from `size_of` — the table is not trusted as written.
- **V2** `count`/`sum`/`min`/`max` stay exact across the cap boundary.
- **V3** Percentiles match a hand-computed reference on a known distribution.
- **V4** Self-time on a hand-built three-level tree equals duration minus retained
  children.
- **V5** Wall-clock union over nested, overlapping, and disjoint intervals.
- **V6** Path aggregation on a known tree; `[?]` + `path_incomplete` when an ancestor
  is excluded by the filter.
- **V7** `group_by` for name / trace / attr.
- **V8** `skip_warmup_ms` drops exactly the intended samples and nulls `exact` with
  the stated reason.
- **V9** `diff` deltas at each percentile, overall and per group.
- **V10** `reset` returns the cleared aggregate and zeroes atomically.
- **V11** Restart → armed, zeroed, `zeroed_by` set, **persisted history intact**.
- **V12** `traces.slow` grouped arm now aggregates the population, not the tail
  (regression test for §1.1).
- **V13** Histogram percentiles match the exact sample percentiles within
  `error_pct` on several distributions (uniform, bimodal, heavy-tailed).
- **V14** `snapshot` records, returns, and zeroes; `reset` discards without
  recording; history reflects exactly the snapshots taken.
- **V15** Snapshot policy round-trip: each snapshot echoes the policy it was taken
  under, and editing the collector's policy does not alter existing snapshots.
- **V16** Merged history equals the sum of its parts for scalars and histograms, and
  omits sample-derived projections with a stated reason.
- **V17** Descriptions (collector and snapshot) appear in read, diff, history, and
  document output.
- **V18** A document carries everything §9.3–§9.7 requires: front-matter with every
  field, the how-to-read note, definition, every snapshot, the computed comparison,
  the full per-name vocabulary, `meta`, the limitations section, and a sidecar
  reference. Histogram buckets **and their layout** are in the sidecar, losing nothing
  relative to `format: "json"`.
- **V19** Noise floor: an arm with repeats reports spread; an arm with one run states
  the floor is unknown rather than omitting the field.
- **V20** The document stays within its size target on a large collector — the bulk
  goes to the sidecar, not the document.
- **V21** Omitting `path` returns the document in the response; supplying it writes
  the same bytes to disk. Neither path is a special case of the other.
- **V22** Regeneration is lossless: render, then re-render with `finding` supplied,
  and nothing but the finding and the render timestamp differs.
- **V23** Body order matches §9.4 — what moved, then what to do next, then detail,
  then vocabulary, then definitions. A reader hitting reference material first is a
  regression, not a formatting preference.

**Adversarial**

- **A1 Cap boundary.** Match counts one under / exactly at / one over the budget.
  `complete` flips correctly and `matched` stays exact across all three.
- **A2 Parent cycle.** Malformed parent chain → depth cap terminates the walk; no
  hang, no unbounded path string.
- **A3 Arrival order.** Child-before-parent, parent-before-child, and interleaved
  traces all produce **identical** projections.
- **A4 Concurrent reset during ingest.** `sum(before) + sum(after)` equals total
  ingested — no lost update, no torn read.
- **A5 Domain rebinding mid-collection.** The collector keeps collecting from its
  pinned domain (§4.2).
- **A6 High-cardinality group key.** Intern cap hit → `__overflow__`,
  `cardinality_capped: true`, memory bounded.
- **A7 Mismatched diff.** Different filters and different levels are both refused.
- **A8 Incomplete diff.** Refused by default; permitted with `allow_incomplete`.
- **A9 Idle CPU.** Collector armed with no traffic → zero CPU (the standing
  wake/sleep contract).
- **A10 Truncation must not launder into a snapshot.** Overflow the sample budget,
  then snapshot. Histogram percentiles must still describe the whole run;
  sample-derived projections must carry `complete: false` through into the snapshot.
  This is the defect that moving the histogram to ingest exists to prevent, so it
  gets a test rather than an argument.
- **A11 Snapshot during ingest.** Spans arriving concurrently with `snapshot` land
  in exactly one side — `snapshot.count + live.count` equals total ingested, with no
  span counted twice or dropped.
- **A12 History eviction.** Exceed `max_snapshots`; the oldest go, `snapshots_evicted`
  is reported, and no surviving snapshot is corrupted by the eviction.
- **A13 Diff across representations.** Snapshot (estimated) against live (sampled) —
  the diff compares at the weaker representation and says so, rather than silently
  mixing the two.
- **A14 Observer effect — the capability A/B on the feature itself.** Run the full
  suite with collectors armed and with them absent, and measure ingest overhead.
  A profiler that measurably perturbs what it measures is broken regardless of how
  correct its arithmetic is. This is the one test that cannot be replaced by
  inspection.

---

## 13. Build order

Each phase ends with targeted tests green and a fresh-eyes self-review of the phase
diff. Mid-checkpoint after phase 1.

1. **Core.** Levels, columnar sample buffer, exact tier **including the histogram**,
   ingest hook, domain-keyed registry, `add`/`list`/`get`/`reset`/`remove`,
   descriptions, the `ProfileResult` shape with its three categories,
   `traces.profile`. Includes the §1.1 `traces.slow` fix, since the grouped arm
   becomes a wrapper over the projection core.
2. **Projections.** Path aggregation, wall-clock union, warm-up exclusion,
   `group_keys`.
3. **History.** `snapshot` / `history` / `edit`, snapshot policy with
   definition-time validation, persistence of snapshots, merging.
4. **Comparison.** `collectors.diff` with its refusals and cross-representation
   handling; `collectors.document` in all three formats.
5. **Guards.** Threshold triggers with rolling-window evaluation.

Phases 1–3 are the complete driving workflow. If phases 4–5 are cut or deferred, the
feature still does the job it was designed for — the ordering is chosen so that cut
is clean.

Docs are part of done, in the same session: README tool table + a Filter-DSL/profiling
section, the logmon skill's tool-selection table, and deletion of the stale
"span triggers don't exist" roadmap line.

**Deferred** (revisit when wanted twice): log-derived durations, persisted baselines,
multi-key `group_by`, reservoir sampling as an opt-in percentile-only mode.

---

## 14. Design gate — lens set

T2 takes one fresh-context pass, but the two canonical lenses are the floor, and this
design's failure surface argues for more. Named now so the gate brief is not
improvised:

- **Buildable** (canonical) — are the types, columns, and projection interfaces
  defined well enough to build without guessing?
- **Sound** (canonical) — hazards, missed failure modes, wrong approach.
- **Performance** — this adds work to a hot ingest path. Warranted by §4.2.
- **Prior-art / reference** — self-time, folded stacks, and percentile-from-sample
  are solved problems in mature profilers. Reconcile against how they do it rather
  than re-deriving; the standing rule is to port a proven structure verbatim and
  layer the new concern on top.
- **False-positive** — §5.6 introduces two *blocking* refusals, and §7 adds a third
  (`edit` refusing filter/level changes). A blocking check must reject only the
  provable, or it gets routed around and stops protecting anything. Verify all three
  fire only on recorded facts.
- **Reader** — hand a synthetic document to a fresh agent with **no session context**
  and ask what it can and cannot conclude from it. §9 claims the document is
  interpretable cold; that claim is only testable by trying it, and inspection cannot
  substitute. Run it for both moments: immediately-after-a-run, and cold months later.
- **Durability** — snapshots persist (§10), which puts user-visible measurement
  history on disk for the first time in this feature. Not a migration, so not T3, but
  the restore path deserves the same scrutiny: a snapshot that restores subtly wrong
  is worse than one that fails to restore, because it still reads like a measurement.

Convergent findings across lenses are to be treated as load-bearing, not coincidence.
