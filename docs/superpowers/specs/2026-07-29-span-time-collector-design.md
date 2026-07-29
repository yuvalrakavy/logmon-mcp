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
| Per-trigger debounce | `post_window_remaining` (`crates/core/src/daemon/session.rs`) | Landed in 0.3.0. Threshold triggers (§7) reuse it rather than inventing cooldown. |
| File export | `logs.export` | Establishes the pattern `collectors.export` (§8) follows. |
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

1. Collector object: create/arm, list, read, reset, remove — per session.
2. Two-tier retention: an always-exact scalar tier and a capped columnar sample tier.
3. Retention levels (`scalar` / `timing` / `tree`) chosen at definition time.
4. Optional `group_keys` — span attributes retained as grouping dimensions.
5. Read-time projections: sum/count/avg/min/max, exact percentiles, self-time,
   wall-clock union, nested-match detection, grouping by name / trace / attribute /
   **call path**, and warm-up exclusion.
6. `traces.profile` — the same projection over the ring buffer, no collector needed.
7. `collectors.diff` — deltas at every percentile, with mismatch refusals.
8. `collectors.export` — folded-stack output for speedscope / flamegraph.pl.
9. Threshold triggers on collectors (phase 4).
10. Fixing the `traces.slow` grouping bias (§1.1).

**Non-goals**

- Log-derived durations (aggregating a numeric GELF field for non-OTLP apps).
  Broadens reach, but it is a parallel ingest path and nothing in the driving case
  needs it. Deferred (§12).
- Persisted baselines ("is this slower than last week"). The exact tier is small
  enough to persist, but it needs its own storage surface and no current use wants
  it. Deferred (§12).
- Multi-key `group_by`. Single dimension in v1.
- Reservoir sampling. Explicitly rejected — see §3.4.
- Any change to how spans are received or stored.

---

## 3. Data model

### 3.1 Two retention tiers

**Exact tier** — `{count, sum_ms, min_ms, max_ms}` for the collector as a whole, plus
the same four per span name. Memory is O(distinct span names), which is small and
bounded by real span vocabularies. **Never capped, never approximate.** This tier
carries the headline A/B number, so the figure quoted in "cache on vs cache off"
stays exact regardless of run length.

**Sample tier** — one record per matched span, stored **columnar** (struct-of-arrays,
one `Vec` per column). Columnar rather than array-of-structs for two reasons: no
alignment padding around the `u128` trace id, and levels fall out naturally as
"which columns are allocated" instead of a variant layout.

### 3.2 Retention levels

Ordered, each a superset of the previous. Chosen per collector at definition time.

| Level | Columns added | Unlocks | Bytes/match |
|---|---|---|---|
| `scalar` | *(none — exact tier only)* | count, sum, avg, min, max — total and per span name | 0 |
| `timing` | `start_ns i64`, `end_ns i64`, `name_id u32`, `flags u8` | exact percentiles, wall-clock union, per-name percentiles, duration histograms, warm-up exclusion | 21 |
| `tree` **(default)** | + `span_id u64`, `parent_span_id u64`, `trace_id u128` | self-time, nested-match detection, per-trace rollups, call-path aggregation, folded export | 53 |

Plus **4 bytes per match per `group_keys` entry** at `timing` and above (one interned
`u32` column each). At `scalar` there is no sample tier and therefore no column —
group keys instead widen the **exact tier's key**, which becomes
`(span name × group-key values)`. Bounded by the §3.3 cardinality cap, so still
O(vocabulary) rather than O(matches).

> Byte figures are **derived from field sizes, not measured.** They must be
> re-derived against `size_of` in the phase-1 tests (§10, V1), not trusted from
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

This also fixes the baseline for A10 (§11). logmon's span ingest is already heavier
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
  "filter": "sv=store_server",
  "level": "tree",
  "matched": 48213,                  // exact tier — every match, always
  "window": { "armed_at": "...", "read_at": "...", "wall_ms": 812004.0 },

  "exact": {                         // null iff warm-up exclusion is active (§5.5)
    "count": 48213, "sum_ms": 1234567.8,
    "avg_ms": 25.6, "min_ms": 0.1, "max_ms": 4310.2
  },

  "sampled": {
    "complete": true,                // false ⇒ prefix-truncated at the budget
    "sample_count": 48213,
    "p50_ms": 12.0, "p80_ms": 41.2, "p95_ms": 180.4, "p99_ms": 902.1,
    "self_time_ms": 998001.2,        // tree
    "wall_union_ms": 310221.0,       // timing
    "nested_matches": 0,             // tree
    "error_count": 12
  },

  "groups": [ { "key": "...", "exact": {...}, "sampled": {...} } ]
}
```

A cap-degraded percentile cannot be quoted as if it were the exact sum, because the
two live in different objects and `sampled.complete` sits immediately beside the
numbers it qualifies.

Percentile list is a parameter; default `[50, 80, 95, 99]`.

### 5.2 Metric catalogue and minimum level

| Metric | Min level | Notes |
|---|---|---|
| `count`, `sum_ms`, `avg_ms`, `min_ms`, `max_ms` | `scalar` | Exact tier. Never degraded. |
| `p*_ms` | `timing` | Nearest-rank over the retained sample — exact for that sample, which is the whole population unless `sampled.complete` is `false`. |
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

---

## 6. Contract surface

Following the existing `noun.verb` convention (`triggers.add`, `traces.slow`).

| JSON-RPC | MCP tool | Purpose |
|---|---|---|
| `collectors.add` | `add_collector` | Define and **arm immediately**. Params: `name`, `filter`, `level`, `group_keys`, `max_sample_bytes`, `threshold` (§7). |
| `collectors.list` | `get_collectors` | Definitions + live counters + memory used. |
| `collectors.get` | `get_collector` | Read a `ProfileResult`. Projection params: `group_by`, `percentiles`, `skip_warmup_ms`. Non-destructive. |
| `collectors.reset` | `reset_collector` | **Returns the aggregate it just cleared**, then zeroes. |
| `collectors.remove` | `remove_collector` | |
| `collectors.diff` | `diff_collectors` | §5.6 |
| `collectors.export` | `export_profile` | §8 |
| `traces.profile` | `profile_spans` | Same projection over the ring buffer. Accepts bookmark windows (`b>=`/`b<=`) — already supported on span filters. |

`collectors.reset` returning the cleared aggregate makes run→read→zero→run atomic:
there is no window in which a read has happened, the zero has not, and a straggling
span lands in the wrong arm.

Wire discipline: new types are additive; `cargo xtask verify-schema` regenerates
`protocol-v1.schema.json` and the result is committed in the same change.

CLI mirrors the MCP surface 1:1, per the project's standing rule
(`logmon-mcp collectors add …`, `logmon-mcp collectors get <name> --json`).

---

## 7. Threshold triggers (phase 4)

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

## 8. Folded-stack export

`collectors.export(name, format, path)`. `format: "folded"` emits
`nameA;nameB;nameC <self_time_us>` — the §5.4 path projection serialised
differently, read directly by speedscope and flamegraph.pl. `format: "json"` writes
the `ProfileResult`. Requires `tree` for folded.

The value is division of labour: the assistant reads the table, the human looks at
the shape, and shapes are where the unqueried problem shows up. Follows the existing
`logs.export` file-export pattern.

---

## 9. Lifecycle, persistence, restart

- **Definitions persist** for named sessions, exactly as triggers/filters/bookmarks
  do. Anonymous-session collectors die with the session.
- **Accumulated data does not survive a daemon restart** — the sample tier is up to
  the memory budget and is not written to disk.
- On restore, a collector comes back **armed but zeroed**, carrying
  `zeroed_by: "daemon_restart"` and `zeroed_at` until the next explicit reset.
  A restart mid-A/B is therefore *visible in the result*, not a silently partial run
  that reads like a complete one.
- A collector is pinned to its creation domain (§4.2).
- Duplicate collector name within a session → error, mirroring `rename_session`'s
  refusal to let two live identities collide.

---

## 10. Error handling

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
| Sample budget hit | `sampled.complete = false`; exact tier unaffected |
| Zero matches | Well-formed empty result — no sentinel, no division by zero |

---

## 11. Test list (verification + adversarial)

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
- **V11** Restart → armed, zeroed, `zeroed_by` set.
- **V12** `traces.slow` grouped arm now aggregates the population, not the tail
  (regression test for §1.1).

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
- **A10 Observer effect — the capability A/B on the feature itself.** Run the full
  suite with collectors armed and with them absent, and measure ingest overhead.
  A profiler that measurably perturbs what it measures is broken regardless of how
  correct its arithmetic is. This is the one test that cannot be replaced by
  inspection.

---

## 12. Build order

Each phase ends with targeted tests green and a fresh-eyes self-review of the phase
diff. Mid-checkpoint after phase 1.

1. **Core.** Levels, columnar sample buffer, exact tier, ingest hook, domain-keyed
   registry, `add`/`list`/`get`/`reset`/`remove`, the `ProfileResult` shape,
   `traces.profile`. Includes the §1.1 `traces.slow` fix, since the grouped arm
   becomes a wrapper over the projection core.
2. **Projections.** Path aggregation, wall-clock union, warm-up exclusion,
   `group_keys`.
3. **Comparison.** `collectors.diff` with its refusals; folded export.
4. **Guards.** Threshold triggers with rolling-window evaluation.

Docs are part of done, in the same session: README tool table + a Filter-DSL/profiling
section, the logmon skill's tool-selection table, and deletion of the stale
"span triggers don't exist" roadmap line.

**Deferred** (revisit when wanted twice): log-derived durations, persisted baselines,
multi-key `group_by`, reservoir sampling as an opt-in percentile-only mode.

---

## 13. Design gate — lens set

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
- **False-positive** — §5.6 introduces two *blocking* refusals. A blocking check
  must reject only the provable, or it gets routed around and stops protecting
  anything. Verify both refusals fire only on recorded facts.

Convergent findings across lenses are to be treated as load-bearing, not coincidence.
