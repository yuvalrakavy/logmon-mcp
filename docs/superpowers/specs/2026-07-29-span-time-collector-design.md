# Span Time Collector — Design Spec

**Date:** 2026-07-29 (rev 2 — post design-gate)
**Status:** DESIGN — gate run with four fresh-context lenses; blockers resolved in this
revision. Changed mechanisms need a re-gate before build (see §15).
**Tier:** **T2** — novel feature minting externally-binding contract surface (JSON-RPC
methods, MCP tools, wire types in `protocol-v1.schema.json`).

> ## Design-gate resolution log (rev 1 → rev 2)
>
> Four blind lenses: buildability, soundness + false-positives, performance + prior-art,
> and a **cold reader** given only a synthetic document and no other context.
>
> The design's *structure* was confirmed by every lens — read-time projections, prefix
> truncation over reservoir sampling, ingest-fed histograms, reset-vs-snapshot
> separation, the §9.7 sidecar reversal, and self-time-per-folded-line all survived.
> What failed were claims made **about** that structure. Convergent findings, weighted
> hardest per the gate's own rule:
>
> - **★ Nesting (3 lenses, 3 facets).** `sum_ms` double-counts a matched span nested in
>   another matched span; the broad-filter idiom §5.4 recommends *guarantees* nesting;
>   and it overstates a cache win precisely because caches remove nested work. The cold
>   reader independently derived the invariant `nested_matches == 0 ⟹ sum == self_time`
>   and caught rev 1's own §5.1 example violating it. Resolved: **cumulative and self
>   are named peers** (§5.1, §5.2), nesting undetected below `tree` is stated not
>   implied, and negative self-time now has a rule (§5.3).
> - **★ `matches_pattern` allocates (3 lenses).** rev 1 §4.2 claimed matching allocates
>   nothing; it is `text.to_lowercase().contains(&s.to_lowercase())` — two heap
>   allocations per pattern per span. Corrected in §4.2, with the fix scoped.
> - **★ `group_keys` on non-string attributes (3 lenses).** The matcher's attribute arm
>   is `.and_then(|v| v.as_str())`, so a boolean `cache.enabled` — the flagship
>   example's most likely encoding — matches nothing. Resolved in §3.3.
> - **★ The exact tier was never exact (soundness).** Spans are dropped at the receiver
>   before `process_span_for_domain` runs, and drop rate scales with load — the variable
>   under test in every A/B this feature exists to run. Resolved: `ingest` accounting in
>   the result shape (§5.1), a §9.6 row, and `trustworthy: false` on any drop.
> - **★ Order-independence was false (soundness).** rev 1 §4.3 claimed arrival order
>   never affects a result. Under truncation, *which spans land in the prefix is
>   arrival order*. Restated in §4.3; A3 split so it stops testing only the trivially
>   true case.
> - **★ All four blocking checks had legitimate false positives (soundness).** The worst
>   blocked the driving use case. The design's idiom everywhere else is
>   *mark-don't-block*; the refusals were an unargued exception. Re-scoped in §5.6/§7.
> - **Locking (performance + soundness).** A4/A11's atomicity demands forced a coarse
>   lock that O(N) read projections would hold, stalling ingest into the drop path.
>   Resolved with a chunked sample tier (§3.6).
> - **The histogram was named, not chosen (prior-art).** Algorithm, unit, range, and
>   layout were all unspecified, and the ~4 KB figure was 2–3.5× low. Pinned in §3.5.
> - **A14 could not detect what it existed to detect (performance).** Ingest is behind a
>   65 536-deep `try_send` channel with no backpressure onto the system under test.
>   Redesigned in §12.
> - **`logs.export` writes no file daemon-side (buildability)**, and §8's "reuse the
>   existing debounce" was wrong three ways. Both corrected.
>
> Two defects found in **shipped** code were filed separately rather than fixed here:
> the per-span allocation/re-parse in the trigger path, and OTLP/gRPC reporting success
> while dropping spans mid-batch.

---

## 0. Motivation

logmon answers *"what happened?"*. It does not answer *"where did the time go, and did
my change help?"*.

The driving case: measuring a cache behind a kill-switch by running a suite twice and
comparing time spent in a class of spans — the **capability A/B** that ways-of-working
requires of any optimisation behind a kill-switch, and which logmon cannot support at
all today.

The generalisation: a **span time collector** that accumulates spans matching a filter
and lets the consumer ask any timing question of the result.

### 0.1 The principle that shaped everything

**Collect broadly; decide at read time.** The collector computes no metrics. It retains
a compact record per matched span, and every metric is a read-time projection. Retaining
`span_id` + `parent_span_id` makes self-time a projection rather than ingest-time
bookkeeping, which removes the arrival-order hazard that made self-time unattractive.

The gate's headline finding is where rev 1 failed to *follow* this principle: it left
the double-counting quantity as the number the document leads with.

---

## 1. Current architecture — the seams we build on

Verified against the tree at `a08c119`. Every row re-checked during the gate; two rev-1
rows were wrong and are corrected here.

| Seam | Where | Status |
|---|---|---|
| Span ingest hook | [`process_span_for_domain`](../../../crates/core/src/daemon/span_processor.rs) | **Confirmed.** Stores the span, then evaluates span-filter triggers per bound session. Span triggers **do** exist — the README roadmap line saying otherwise is stale and is deleted in this change. |
| Span matching | `matches_span` (`crates/core/src/filter/matcher.rs`) | **Confirmed with two caveats.** Supports `sn`, `sv`, `st`, `sk`, `d>=`/`d<=`, bare patterns, and attributes — but attributes are **string-only** (§3.3), and matching **allocates** (§4.2). |
| Bookmark windows on spans | `SeqFilter` arm in `matches_span_qualifier` | **Confirmed.** Bookmarks resolve to `SeqFilter` at the RPC layer; spans and logs share one seq counter. `traces.profile` gets window-scoping free. Note a collector pre-parses at arm time, so a bookmark in a *collector* filter resolves **once, at arm** — different semantics from `traces.profile`, and a reason §7 rejects them there. |
| Per-session state registry | `SessionRegistry` | **Confirmed.** Collectors follow the triggers/filters/bookmarks pattern for lifecycle. |
| ~~Per-trigger debounce~~ | — | **rev-1 error, retracted.** The cited `post_window_remaining` is the session-level *storage* window. Per-trigger firing suppression is `Trigger::post_remaining`, it counts **entries** not milliseconds, and `record_match` states outright that span triggers are *not* debounced. §8 reuses nothing. |
| ~~`logs.export` file pattern~~ | — | **rev-1 error, retracted.** The RPC takes no path and writes nothing; the writes are client-side. It does establish the right *division of labour* — see §9.9. |
| Span ring buffer | `SpanStore`, `span_buffer_size` default **10 000** | **Confirmed**, and eviction is silent. |
| Receiver drop accounting | `ReceiverMetrics` / `ReceiverDropSnapshot` | **Newly load-bearing.** `try_send_span` drops on a full 65 536-slot channel and counts it. §5.1 now surfaces this. |

### 1.1 Two existing defects this work touches

- **`traces.slow` `group_by="name"` aggregates a biased sample.** `slow_spans` filters
  to `duration_ms >= min_duration_ms`, sorts descending, **truncates to `count`**, and
  only then does the handler group. Both truncations bias it — the gate noted rev 1
  blamed only the `count` one. The fix aggregates the **full matching population**;
  `min_duration_ms` becomes a *display* floor applied after aggregation, never before.
  Its nearest-rank convention must also match §5.2 (today `floor(n·0.95)` returns the
  maximum at n=20). **Fixed in-scope.**
- **The trigger path re-parses and allocates per span.** Confirmed in detail (§4.2).
  **Not fixed here** — it has its own blast radius through trigger semantics. Filed
  separately.

---

## 2. Scope

**In scope**

1. Collector object: arm, list, read, snapshot, reset, remove — per session.
2. Two-tier retention: an always-exact tier (integer-nanosecond scalars + duration
   sketch) and a capped **chunked** columnar sample tier.
3. Retention levels (`scalar` / `timing` / `tree`) chosen at definition time.
4. `group_keys` — span attributes retained as grouping dimensions.
5. Read-time projections: cumulative and self time as named peers, percentiles
   (sketch-estimated whole-run, and sample-exact where complete), wall-clock union,
   nesting detection, grouping by name / trace / attribute / **call path**, warm-up
   exclusion.
6. **Ingest-loss accounting** — drops that happen upstream of the collector are
   surfaced, never laundered.
7. Snapshot history with a declared policy, and merging (§6).
8. Descriptions and `meta` on collectors and snapshots.
9. `traces.profile` — the same projection over the ring buffer.
10. `collectors.diff` — deltas at every percentile, with mismatch **marking**.
11. `collectors.document` — the reader-derived artefact (§9).
12. Threshold triggers (phase 5), built from scratch (§8).
13. Fixing the `traces.slow` bias (§1.1).

**Non-goals**

- **Re-importing a document.** No `collectors.import`, no frozen-collector state. The
  consumer is a reader doing the comparison themselves (§9.1).
- **Cross-collector baselines** — a shared store comparable across projects or
  machines. Snapshot history covers the in-collector case. Deferred (§13).
- Log-derived durations for non-OTLP apps. Deferred (§13).
- Multi-key `group_by`; reservoir sampling (rejected, §3.4); any change to how spans are
  received or stored.

---

## 3. Data model

### 3.1 Two retention tiers

**Exact tier** — per collector, per span name, and per group:

| Field | Notes |
|---|---|
| `count` | |
| `cumulative_ns` | `u64` **integer nanoseconds**. f64 addition is not associative, so an f64 accumulator makes results depend on arrival order at the bit level. Integer nanoseconds makes "never approximate" literally true. Reported as `cumulative_ms` (derived f64). |
| `min_ns`, `max_ns` | |
| `error_count` | Moved here from the sample tier: it is an O(1) counter, and under-reporting errors is the least acceptable degradation available. |
| duration sketch | §3.5 |

Memory is O(distinct span names × group cardinality). **Never capped by the sample
budget**; its own limits are §3.5.

`cumulative_ns` is the sum over all matched spans and **double-counts a matched span
nested inside another matched span**. That is a property of the quantity, not a defect,
and §5.1 names it accordingly.

**Sample tier** — one record per matched span, stored columnar in **chunks** (§3.6).

### 3.2 Retention levels

| Level | Columns added | Unlocks | Bytes/match |
|---|---|---|---|
| `scalar` | *(exact tier only)* | count, cumulative, avg, min, max, error count, **estimated percentiles** | 0 |
| `timing` | `start_ns i64`, `end_ns i64`, `name_id u32`, `flags u8` | sample-exact percentiles, wall-clock union, warm-up exclusion | 21 |
| `tree` **(default)** | + `span_id u64`, `parent_span_id u64`, `trace_id u128` | **self time**, nesting detection, per-trace rollups, call-path aggregation, folded output | 53 |

Plus 4 B per match per `group_keys` entry at `timing`+. At `scalar` group keys widen the
exact tier's key instead — and, correcting a rev-1 leak the gate found, **the exact
tier's key is widened at every level**, so `groups[].exact` is genuinely exact
everywhere (§5.1).

`flags` packs status (2 bits) and kind (3 bits). `parent_span_id == 0` means root —
safe, because `bytes_to_span_id` maps all-zero to `None`, so 0 is never a real id.

> Byte figures are **derived from field sizes, not measured**, and must be re-derived
> against `size_of` in phase 1 (§12, V1). The gate checked the arithmetic and it is
> self-consistent, but a re-derivation is still the test.

**Nesting below `tree` is undetectable.** `self_ms` and `nested_matches` both need the
parent columns. At `scalar` and `timing` a result reports `nesting: "undetected"` and
labels its cumulative figure accordingly — it never prints a bare sum that a reader
could mistake for total work (§5.1).

### 3.3 `group_keys`

Span attribute names whose values are interned per collector. At `timing`+ they are a
`u32` column per match, enabling per-group percentiles; at every level they widen the
exact tier's key.

**Value rendering — the gate's flagship failure.** `SpanEntry.attributes` is
`HashMap<String, serde_json::Value>` holding **typed** JSON, and the matcher's attribute
arm does `.and_then(|v| v.as_str())`. A kill-switch emitted as an OTel `BoolValue` — the
normal encoding, and the driving example's own case — matches nothing and would intern
nothing, silently collapsing a two-arm A/B into one group.

Resolution: `group_keys` reads attribute values **independently of the matcher**, with a
declared rendering — `Bool` → `"true"`/`"false"`, `Number` → its shortest round-trip
decimal, `String` → itself, `Array`/`Object` → excluded with a stated reason. A span
**missing** the key falls into a literal `__absent__` bucket, which under a broad filter
is most spans and must not be silently dropped.

The matcher's own string-only limitation is noted as a carve-out, **not** fixed here: it
is shared with the log path and changing it is a filter-semantics change.

**Cardinality.** Per-key cap (default 1024). Past it values fold into `__overflow__` and
the result carries `cardinality_capped: true` naming the key.

### 3.4 Cap behaviour, and why not reservoir sampling

Per-collector `max_sample_bytes` default **64 MiB** (≈1.27 M matches at `tree`);
daemon-wide `max_total_sample_bytes` default **256 MiB**, enforced as a **reservation**
at arm time — which means four default-sized collectors is the practical ceiling across
all sessions and domains, and `collectors.add` says so when it refuses.

The budget is measured against **allocated capacity**, not retained payload. With
chunked columns (§3.6) the two differ by at most one partial chunk, so the cap is a real
bound rather than an under-count of up to 2×.

On reaching the budget the sample tier **stops retaining** — prefix truncation. The exact
tier and the sketch keep running.

**Reservoir sampling is rejected.** It gives unbiased percentiles but breaks the
projections needing *completeness*: self time needs a span's children present, wall union
needs every interval. Prefix truncation keeps every projection valid *for the retained
prefix* — a defensible statement — where a reservoir would make self time quietly wrong.

Prefix truncation is cold-biased, which is why `complete: false` is structural. The
sketch (§3.5) is what keeps that bias away from percentiles: fed at ingest, it covers
every match regardless of the cap.

### 3.5 The duration sketch

**Algorithm: DDSketch**, relative accuracy γ configured for 1 % error, **sparse** store.
Chosen over HdrHistogram for a reason specific to this design: there is one sketch *per
span name*, and HdrHistogram preallocates a dense counts array sized to the full range,
so every narrow per-name distribution pays full price. DDSketch allocates only occupied
buckets, merges exactly, and has a defined protobuf encoding.

> **Phase-1 probe, not an assumption:** confirm a viable Rust DDSketch crate before
> building. If none is, fall back to `hdrhistogram` **with a reduced per-name range**,
> and re-derive the memory table below — do not keep the DDSketch numbers.

**Input unit: integer nanoseconds**, range 1 ns – 1 h. rev 1 left this unspecified, and
the gate found that a millisecond-fed sketch collapses every sub-millisecond span into
one bucket with no error guarantee — i.e. it would destroy exactly the population a cache
creates.

**Memory.** rev 1's "~4 KB" was derived from nothing and is 2–3.5× low for a dense
layout. A sparse sketch is occupancy-driven, so the figure is a **budget, not a
constant**: `max_histogram_bytes` (default 32 MiB per collector) is charged separately
from the sample budget, and the per-name sketch count is capped at
`max_name_sketches` (default 256) with the same `__overflow__` treatment group keys get.
rev 1 capped group cardinality and group sketches but left the **span-name** axis — the
one most likely to explode, since inlining ids into span names is a common OTel
antipattern — with neither cap nor error.

**Why at ingest.** A sketch built later from the sample tier inherits its truncation, and
a long run would be laundered into a clean-looking snapshot. Fed at ingest it is complete
by construction. This is the single reason percentiles survive truncation *and*
snapshotting.

**Layout identity travels with the buckets.** Every sketch carries
`{algo, accuracy, unit, range}` in results, snapshots, documents, and persisted state.
Merging or diffing across mismatched layouts is arithmetically wrong, so it is refused
(§5.6) — the one place a hard block is provable from a recorded fact.

**Axis restriction — stated, because rev 1 over-claimed.** Sketches exist per collector,
per name, and per group. Any projection **outside that key space** — `group_by: "path"`,
`group_by: "trace"`, a duration band, read-time narrowing on any other axis — has no
sketch and must fall back to `sampled`. rev 1 said percentiles are "whole run always";
that is true only on the declared axes. Off-axis, `estimated` is `null` with
`estimated_unavailable_reason`, so the change of provenance is visible.

### 3.6 Chunked sample tier — the lock discipline

Columns are stored as a list of **sealed immutable chunks** plus one **active mutable
chunk** (65 536 records).

- **Ingest** takes a short lock on the active chunk only. Sealing appends an `Arc` to the
  sealed list.
- **Reads** take the lock only long enough to clone the sealed-chunk `Arc` list and copy
  the active chunk, then project **outside** the lock.
- **`snapshot(reset=true)`** swaps the whole structure under the lock and projects
  outside it.

rev 1 said nothing about mutation locking, and the gate showed that A4/A11's atomicity
requirements would otherwise force a single coarse write lock held across an O(1.27 M)
projection — with an O(n log n) interval merge inside it. That stalls the span processor,
the 65 536-slot channel fills, and spans are dropped **at the receiver**, where the
collector cannot see them. The mechanism protecting the arm boundary would have been the
mechanism losing spans at the arm boundary.

---

## 4. Ingest path

### 4.1 Hook

A third step in `process_span_for_domain`, after storing and after trigger evaluation:
evaluate collectors for this domain.

### 4.2 Cost discipline — corrected

rev 1 claimed "matching allocates nothing." **False**, and it was the load-bearing claim.
`matches_pattern` is:

```rust
Pattern::Substring(s) => text.to_lowercase().contains(&s.to_lowercase()),
```

Two heap allocations per pattern per span. `sv=store_server` — the spec's own flagship
filter — is a `Substring`. `sk=` adds a `format!("{:?}", …)`; `st=` clones. Only
`d>=`/`d<=` and `SeqFilter` are allocation-free.

`parse_filter` already lowercases substrings, so the matcher's second lowercase exists
only to cover the JSON-deserializer path. **In scope for phase 1:** lowercase in
`Pattern`'s `Deserialize`, then make the matcher use a non-allocating case-insensitive
comparison. This touches code the log path shares, so it is called out as a deliberate
shared-code change rather than a local optimisation.

Everything else the collector does on the hot path:

- Filters **pre-parsed** at `collectors.add`; no parse per span.
- Domain-keyed registry read under one lock.
- Append to the active chunk (§3.6); amortised O(1).
- Sketch update: O(1), no steady-state allocation. Note DDSketch's sparse store
  allocates on **first** occupancy of a bucket — bounded and self-limiting, but not
  literally never.
- Interning: hash lookup returning an existing `u32` in the steady state.

**The existing path's cost, re-audited.** rev 1's "3+ locks, N+M allocations, a full
parse" was directionally right and materially understated. It omitted the largest cost —
`store.insert(span.clone())` is a deep clone of `SpanEntry` including `String`s, an
attribute map, and an events vector, under a `SpanStore` **write** lock. It also missed
that `is_span_filter_str` parses *every* trigger and the body parses span-filter triggers
*again*, and that the default seeded trigger is a regex — so **the daemon compiles a
regex once per span per bound session today**. Lock growth is `2 + N_registry + 2×S_bound`.

This is the honest baseline for A14 (§12). Not fixed here; filed separately.

### 4.3 Ordering, clocks, and where order-independence actually holds

rev 1: *"span arrival order does not affect any result."* **False, and the gate found the
counterexample in the spec itself:** under prefix truncation, *which* spans are retained
is determined entirely by arrival order. So whenever `complete == false`, every
sample-derived projection is order-dependent.

Corrected statement: **arrival order does not affect any result while
`sampled.complete` is `true`.** The exact tier and the sketch are order-independent
unconditionally — the former because it accumulates integer nanoseconds, the latter
because sketch insertion is commutative.

`start_ns`/`end_ns` come from the span's own timestamps — the **producer's** clock. The
collector's window is broker clock and is reported separately. Warm-up exclusion is
defined relative to `min(start_ns)` over the matched set (§5.5), producer clock
throughout, so the two are never mixed.

**Malformed timestamps.** The OTLP HTTP path defaults missing timestamps to 0, so a
malformed span can sit at the Unix epoch with a zero or negative duration. Such spans are
counted in `count`, **excluded** from the sketch, from wall-union, and from warm-up
origin computation, and reported as `malformed_timestamps: n`.

---

## 5. Read-time projections

One projection module consumes sample records and produces a `ProfileResult`, fed either
by a collector's chunks or by a ring-buffer scan (`traces.profile`).

### 5.1 Result shape

```jsonc
{
  "collector": "cache-ab",
  "description": "Store suite — schema-cache A/B",
  "filter": "sv=store_server",
  "level": "tree",
  "matched": 48213,
  "nesting": "detected",              // "detected" | "undetected" (below tree)
  "window": { "armed_at": "…", "zeroed_at": "…", "read_at": "…", "wall_ms": 812004.0 },

  "ingest": {                        // NEW — spans lost before the collector saw them
    "drops_in_window": 0,            // ReceiverMetrics delta over the window
    "by_source": {},
    "malformed_timestamps": 0
  },

  "exact": {                         // null iff warm-up exclusion is active (§5.5)
    "count": 48213,
    "cumulative_ms": 1234567.8,      // sums nested spans more than once — see below
    "avg_ms": 25.6, "min_ms": 0.1, "max_ms": 4310.2,
    "error_count": 12
  },

  "estimated": {                     // sketch; null off-axis with a reason (§3.5)
    "axis": "collector", "error_pct": 1.0,
    "p50_ms": 12.0, "p80_ms": 41.0, "p95_ms": 181.0, "p99_ms": 900.0
  },

  "sampled": {                       // absent at scalar
    "complete": true, "sample_count": 48213,
    "self_ms": 998001.2,             // peer of cumulative_ms — see below
    "nested_matches": 1204,
    "overlapping_child_ms": 0.0,     // §5.3
    "wall_union_ms": 310221.0,
    "p50_ms": 12.0, "p80_ms": 41.2, "p95_ms": 180.4, "p99_ms": 902.1
  },

  "groups": [ { "key": "…", "exact": {…}, "estimated": {…}, "sampled": {…} } ]
}
```

**Cumulative and self are peers, and they answer different questions.** This is the
gate's headline change. Mature profilers report both — pprof's *cum* and *flat*, and the
same split in gprof and async-profiler — and users of those tools read these fields
through that lens.

- **`cumulative_ms`** — every matched span's duration, summed. A matched span nested
  inside another is counted at both levels. Exact, always available.
- **`sampled.self_ms`** — each span's duration minus its **retained matched children**.
  Cannot double-count. Requires `tree`, and is sample-derived.

They diverge exactly when it matters. A (100 ms) contains B (60 ms), both matched:
cumulative 160, self 100. A cache that removes B and shortens A to 40 ms gives
cumulative −75 % and self −60 %. The cumulative figure overstates *because* the cache
changed nesting depth, which is what caches do.

Therefore: `nested_matches > 0` means a document must not lead with cumulative alone
(§9.4), and `nesting: "undetected"` means the reader has no way to know — which §9.6
turns into an instruction.

**Four categories, structurally distinct.** `exact` (never approximate, never truncated),
`estimated` (whole run on declared axes, ±`error_pct`), `sampled` (exact for what was
retained), and `ingest` (what never arrived). A degraded number cannot be quoted as an
exact one, because they are different objects and their qualifiers sit beside them.

**`groups[].exact` is genuinely exact** — the exact tier's key is widened by
`group_keys` at every level (§3.2). For `group_by: "trace"` and `group_by: "path"`,
which have no exact tier, `exact` is `null` with `exact_unavailable_reason`.

**`wall_ms` is measured from `max(armed_at, zeroed_at)`**, so a window whose data was
discarded by a reset does not read as a window that collected throughout.

### 5.2 Metric catalogue and minimum level

| Metric | Min level | Tier | Notes |
|---|---|---|---|
| `count`, `cumulative_ms`, `avg_ms`, `min_ms`, `max_ms`, `error_count` | `scalar` | exact | Integer-nanosecond accumulation. |
| `estimated.p*_ms` | `scalar` | sketch | Whole run **on declared axes only** (§3.5). |
| `sampled.p*_ms` | `timing` | sample | Nearest-rank; convention pinned in §5.7. |
| `wall_union_ms` | `timing` | sample | Merged `[start, end)` coverage. |
| `self_ms`, `nested_matches`, `overlapping_child_ms` | `tree` | sample | §5.3. |
| `group_by: "trace"` / `"path"` | `tree` | sample | No exact or estimated tier. |

Requesting a metric above the collector's level returns a **loud error naming the level**
— never a zero.

### 5.3 Self time — the three ways it lies, and what each does about it

`self_ms = duration − Σ(retained matched children)`. Three degradations, all now in-band
rather than in a doc note:

- **Filter-excluded children.** With a narrow filter no children are ever retained and
  `self_ms == cumulative_ms` exactly — a tautology presented as a decomposition. Because
  `nested_matches == 0` is definitionally equivalent to "self time carries no
  information", a result with `nested_matches == 0` **suppresses `self_ms`** and states
  why. Unmatched *descendants* of matched spans are worse than absent: their time is
  silently attributed to the nearest matched ancestor, which reads as that ancestor
  doing the work. §9.6 carries this as its own row with its own remedy.
- **Truncation.** A prefix is not subtree-closed, so boundary parents whose children were
  cut absorb their time. `complete: false` marks it; the document says "a mixture", not
  "self time of the prefix".
- **Ingest drops.** A dropped child silently inflates its parent's self time with
  `complete` still `true`. This is why `ingest.drops_in_window > 0` demotes
  `trustworthy`.

**Negative self time is normal, not exotic.** A child can outlive its parent under
`tokio::spawn` + `tracing` — which is what this repo itself emits — and under
cross-process clock skew. Summed naively, negatives cancel positives and produce a
plausible wrong total. Rule: **per span, clamp `self` at zero and accumulate the
overflow into `overlapping_child_ms`**, reported beside `self_ms`. A non-zero value
means the tree is not properly nested and self time should be read with that in mind.
V4b covers it with a child outliving its parent — rev 1's V4 used a well-nested tree and
could not have caught this.

### 5.4 Call-path aggregation

`group_by: "path"` reconstructs each sample's ancestor chain **within the matched set**
and aggregates self time by path. "`schema.resolve` costs 40 s" versus "40 s, 90 % of it
under `reconcile`" — only the second says what to change.

Safety: depth cap 64 plus a visited set; a walk stopping at an unretained non-root parent
marks the path `[?]` with `path_incomplete: true`. Paths only resolve where ancestors are
matched, so the idiom is a **broad filter plus read-time narrowing** — which is also
precisely why §5.3's unmatched-descendant attribution is load-bearing, and why that
guidance belongs in the document rather than only in the skill docs.

### 5.5 Warm-up exclusion

`skip_warmup_ms: N` drops samples whose `start_ns` is within N ms of **`min(start_ns)`
over the matched set** — producer clock throughout. rev 1 defined it against the broker
clock window, which the gate flagged as conflating the two clocks §4.3 promises never to
mix: with a 5 s skew it would drop everything or nothing.

Because the exact tier and the sketch are both unwindowed, warm-up exclusion sets
**both** `exact: null` and `estimated: null`, each with its reason. rev 1 nulled only
`exact` and left `estimated` including the warm-up samples — while §5.1 told the reader
the two percentile sets should agree within `error_pct`. By §5.5's own argument that
disagreement would be large, and it would appear exactly where the reader had been told
to expect agreement.

### 5.6 `collectors.diff` — mark, don't block

A read-time projection over two results. Reports absolute and relative deltas for
`count`, `cumulative_ms`, `self_ms`, and **each percentile**, overall and per group,
with the noise floor propagated per row (§6.5) and sub-floor deltas suppressed.

**Delta error bounds are computed on the delta, not inherited.** Two estimates at
±1.0 % give roughly ±1.4 % on their difference. rev 1 implied the per-measurement bound
applied to the delta, which is the arithmetic that turns noise into a finding.

Either side may be `collector` or `collector@label`.

The gate found a legitimate wrongly-blocked case for **every** rev-1 refusal, and noted
that the design's idiom everywhere else is *mark-don't-block*. Re-scoped:

| Condition | rev 1 | rev 2 |
|---|---|---|
| Different `level` | Refuse | **Compare at `min(level)`** and say so. Every exact and sketch figure is identically defined at all levels. rev 1 blocked the driving case: arm A at `tree` blows the budget, arm B is re-armed at `timing` to fit, and the `cumulative_ms` comparison — the whole point — was refused. The correct handling was already two paragraphs away ("compare at the weaker of the two"). |
| Different `filter` | Refuse (raw string) | Compare **canonicalized `ParsedFilter`s**, so reordered qualifiers and case differences match. A genuine difference emits `filter_mismatch` on the result plus `allow_mismatch` to proceed. Bookmark-windowed arms differ **by necessity** and must not be blocked. |
| Incomplete side | Refuse whole diff | **Scope to sample-derived rows.** `exact` and `estimated` are provably unaffected by truncation — that is §3.5's entire argument, which rev 1's refusal cancelled out. Sample rows emit null-with-reason. `scalar` collectors, which have no sample tier at all, are explicitly permitted. |
| Mismatched sketch layout | *(absent)* | **Refuse.** Merging or diffing across layouts is arithmetically wrong and the layout is a recorded fact. This is the only new hard block, and the only one that meets the provability bar. |

**Both arms truncated is still refused by default** — not a false positive. A faster arm
reaches the match cap later in wall-clock terms, so the two cold prefixes cover different
slices. `allow_incomplete` remains for the deliberate case.

### 5.7 Percentile convention

**Nearest-rank, 1-indexed: `ceil(p/100 × n)`**, for `sampled`, for the sketch's quantile
function over cumulative bucket counts, and for the repaired `traces.slow`. Stated once,
here, because V3/V12/V13 cannot be written without it and rev 1 left it implicit — the
existing `traces.slow` uses `floor(n·0.95)`, which returns the maximum at n = 20.

---

## 6. Snapshots and history

### 6.1 Why

The workflow is multi-run: arm, run, snapshot, change one variable, run, snapshot.
Without in-collector history the numbers are copied out by hand — the error-prone,
context-losing step the feature exists to remove.

Snapshots are also what make a run durable — **given §10's write-through**, which rev 1
lacked while claiming durability it did not have.

### 6.2 Operations

| Call | Behaviour |
|---|---|
| `collectors.snapshot(name, label?, description?, meta?, reset=true)` | Record, return, and by default zero. |
| `collectors.reset(name)` | **Discard** without recording — the botched-run path. |
| `collectors.history(name, limit?, merge?)` | Descriptors; `merge` combines (§6.5). |
| `collectors.get(name, snapshot=label)` | Read one snapshot. |
| `collectors.diff(a, b)` | Either side `collector` or `collector@label`. |

Unlabelled snapshots auto-label `snapshot-<n>`; the counter **does not reuse numbers
after eviction**, so `@snapshot-3` never silently means a different run. Labels are
restricted to `[A-Za-z0-9._-]` — `@` and `/` would break `collector@label` parsing — and
must be unique within a collector. `max_snapshots` (default 50) evicts FIFO and reports
`snapshots_evicted`.

### 6.3 What a snapshot records

```jsonc
"snapshot": {
  "per_name": true, "per_group": true,
  "projections": ["self_time", "wall_union", "top_paths:100"],
  "raw_sample_bytes": 0
}
```

Always present regardless of policy: the exact tier, the sketches **with their layout
identity**, `ingest` accounting, and — closing a gap the gate found — the collector's
**`filter`, `level`, and snapshot policy at the time it was taken**. rev 1 recorded none
of those, which meant `collectors.diff`'s mismatch check would have read the *live*
collector's current definition rather than the snapshot's: inference presented as proof,
and only self-consistent because a second refusal forbade edits.

`raw_sample_bytes > 0` keeps a **prefix**, with the same `complete` semantics as the live
cap. Not downsampled: uniform downsampling keeps percentiles honest but breaks self time
and paths, which need complete parent–child sets.

### 6.4 Four disciplines

1. **Validated at definition time** — `top_paths` on a `timing` collector errors at
   `collectors.add`, not after a 40-minute run.
2. **Size quoted up front** — `add` returns the derived per-snapshot size and the total
   at `max_snapshots`, computed from the *actual* sketch budget (§3.5), not rev 1's
   unfounded 4 KB.
3. **Each snapshot echoes its own policy, filter, level, and layout**, so editing a
   collector never retroactively rewrites history.
4. **Completeness travels** — a snapshot records the `complete` flag and the `ingest`
   drop count of what it was computed from.

### 6.5 Merging and the noise floor

Exact scalars and sketches are additive, so `history(merge: [labels])` combines runs —
the direct way to beat down run-to-run noise. **Merging is exact only across identical
sketch layouts**; mismatches are refused (§5.6). rev 1 claimed unconditional exactness.

Sample-derived projections do not merge and are omitted with a stated reason.

**The noise floor.** With two or more snapshots of the same configuration, the document
reports min, max, and coefficient of variation across repeats, and **propagates it into
every delta row** rather than mentioning it once in prose. An 8 % improvement above a
2 % floor is a finding; the same 8 % above a 15 % floor is nothing, and the floor is
strictly unrecoverable if not captured at measurement time.

Single-run arms state the floor is **unknown** rather than omitting the field, because an
absent floor reads as a stable one.

### 6.6 Descriptions and `meta`

`description` on the collector; `label` + `description` + `meta` per snapshot. `meta`
carries the provenance logmon cannot infer — commit, build profile, varied configuration
— and is per-snapshot precisely because arms may differ in it.

---

## 7. Contract surface

`noun.verb`, following `triggers.add` / `traces.slow`.

| JSON-RPC | MCP | Notes |
|---|---|---|
| `collectors.add` | `add_collector` | `name`, `filter`, `level`, `group_keys`, `max_sample_bytes`, `max_histogram_bytes`, `description`, `snapshot`, `threshold`. Returns derived size estimates. |
| `collectors.list` | `get_collectors` | Definitions, counters, measured memory. |
| `collectors.get` | `get_collector` | `group_by`, `percentiles`, `skip_warmup_ms`, `snapshot`. Non-destructive. |
| `collectors.snapshot` | `snapshot_collector` | §6.2. Persists write-through (§10). |
| `collectors.history` | `get_collector_history` | §6.5. |
| `collectors.edit` | `edit_collector` | §7.1. |
| `collectors.reset` | `reset_collector` | Discard; returns what it cleared. |
| `collectors.remove` | `remove_collector` | |
| `collectors.diff` | `diff_collectors` | §5.6. |
| `collectors.document` | `document_collector` | §9. Returns bytes; the **client** writes (§9.9). |
| `traces.profile` | `profile_spans` | Ring-buffer projection; accepts bookmark windows. |

**Filter admission.** rev 1 said this "reuses `is_span_filter`". It cannot: that
predicate returns **false** for bare patterns, attribute-only filters, and pure bookmark
windows — all legal span filters, and two of them are the spec's own examples. It was
written as a "should I try this trigger against spans" hint where a false negative is a
silent no-op; as a rejection gate it converts that gap into a loud refusal of valid
input. The correct predicate is **"contains no log-only qualifier"**, which needs new
public parser API — `is_log_filter`, mirroring the private `is_log_qualifier`.

**Bookmarks in a collector filter are rejected**, matching `filters.add` and
`triggers.add`. A collector pre-parses at arm time, so a bookmark would freeze a stale
seq bound for the whole run. `traces.profile` still accepts them.

### 7.1 What `collectors.edit` may change

rev 1 refused all `filter` and `level` changes. The gate found two legitimate blocked
cases, and noted the justification given was vacuous in the first:

- **Typo'd filter, no history.** Arm a collector, see zero matches, fix it. Nothing to
  invalidate — and the prescribed remove-then-add is non-atomic on the name and discards
  the description, policy, and threshold. **Permitted when `snapshots.is_empty() &&
  matched == 0`**, which is the provable form of the check.
- **Raising the level.** A `timing` snapshot correctly describes what `timing` retained.
  Since §6.4 already echoes each snapshot's own level, a raise invalidates nothing.
  **Permitted.**

**Still refused:** changing `filter` on a collector with history, and *lowering* level.
Both would leave stored snapshots described by a definition that does not fit them.

Wire discipline: new types additive; `cargo xtask verify-schema` regenerated and
committed. CLI mirrors MCP 1:1.

---

## 8. Threshold triggers (phase 5) — built, not reused

A collector may carry `threshold: { metric, group?, op, value, window_ms }`.

**rev 1's justification was wrong three ways** and is withdrawn: the cited symbol is the
session-level storage window, the real per-trigger counter measures **entries** rather
than time, and `record_match` states explicitly that span triggers are **not** debounced
because the span path never reaches `evaluate`. Nothing is reused; this is new
machinery.

**Rolling window, evaluated on arrival.** A bucket ring advanced by span arrival — not a
timer — so an idle collector costs nothing and A9's zero-CPU-at-idle contract holds. The
consequence must be stated rather than discovered: **with no traffic a breached threshold
neither fires nor clears.** A threshold is a load-time guard, not a liveness check.

Evaluation is against the rolling window, never the since-arm aggregate, which would
become unmovable as samples accumulate and go silently deaf exactly when a run got long
enough to matter.

---

## 9. Documenting a collector

`collectors.document(names, format, path?, question?, finding?)`.

### 9.1 Two moments, one artefact

- **Now.** The run just finished. The document is the *synthesis* — it assembles what
  would be a dozen scattered `collectors.get` calls into one picture, and shows what
  moved and what to do next.
- **Months later.** A new optimisation idea; the job becomes triage and trust.

Designing for only the second produces an artefact nobody opens until it is too late to
fix the measurement; only the first, one that is uninterpretable once the session's
context is gone.

**A document is a render, not a commit.** `path` is optional; omit it and the document is
returned in the response. Regenerating is free and lossless, which matters because
**`finding` normally arrives after the first read** — you render the document in order to
form the conclusion, then re-render with it attached.

**Not re-imported.** No `collectors.import`. Comparison happens in the reader.

### 9.2 `get` versus `document`

`collectors.get` answers a question you already have and returns one parameterised
projection. `collectors.document` tells you which questions to ask: a fixed synthesis —
comparison, noise floor, vocabulary, limitations, ranked movers. If a future change makes
one a strict subset of the other, that is the signal to remove one.

The name is `document`, not `insights`: logmon computes comparisons, spreads, and
rankings; the insight is the reader's.

### 9.3 The reader's five questions

Each item earns its place by answering one. Anything answering none does not belong.

| The reader asks | When | The document must |
|---|---|---|
| Is this about my thing? | Later | Be triageable **without being read**, across thirty files (§9.4) |
| Can I trust it? Is it comparable? | Later | Carry its environment and its own limits (§9.4, §9.6) |
| What did it conclude? | Both | Record the question and the finding |
| Real or noise? | Both | Quantify run-to-run spread (§6.5) |
| **What should I do next?** | **Now** | Rank what moved; pair every limitation with its remedy |

### 9.4 Triage front-matter

Fixed-schema YAML, `grep`-able across a directory:

```yaml
format_version: 1
collector: cache-ab
question: "Does the schema cache reduce time in store_server spans?"
finding: "…"                    # absent ⇒ explicitly `finding: null`, never omitted
measured_at: 2026-07-29T14:02:11Z
filter: "sv=store_server"
filter_intent: "all server-side work; renderer and panel work excluded"
services: [store_server]
git_sha: { cache-off: a08c119, cache-on: a08c119 }   # per-arm, or `varies`
build_profile: release          # `unknown` if not supplied — never absent
parallelism: 16
host: imac-studio
runs: 4
trustworthy: false
trustworthy_reason: "cache-off single-run; cache-on sample truncated"
sidecar: 2026-07-29-cache-ab.data.json
```

Three fields carry the triage weight: **`question`/`finding`** (data alone is a log; a
recorded conclusion is memory), **`filter_intent`** (a filter string is precise and
nearly useless as a relevance signal months on), and **`build_profile`** (comparing a
debug measurement against a release run is worse than no data, because it looks like
data).

**No field is ever merely absent.** rev 1 stated the principle — "an absent floor reads
as a stable one" — and then broke it by leaving `trustworthy` and `build_profile`
omittable. Every front-matter key is always present, with `null` / `unknown` / `varies`
as explicit values. `git_sha` and `build_profile` are **per-arm**, because a document
spanning multiple arms may span multiple commits and a directory-wide `grep git_sha`
must not return one of several.

`trustworthy` is `false` whenever any arm is single-run, any arm is truncated,
`ingest.drops_in_window > 0`, or `nesting: "undetected"` on a filter that could match
nested spans.

Recommended filename `YYYY-MM-DD-<collector>-<slug>.md`.

### 9.5 Body, in reading order

Most-decision-relevant first, reference material last — and **every caveat travels with
the number it qualifies**, which is the gate's sharpest document finding. rev 1 put the
headline on one line and the reason to disbelieve it forty lines later.

1. **What moved** — the computed comparison, ranked by contribution, **cumulative and
   self side by side**, with each row's error bound and sub-floor deltas visually
   suppressed in place. Ranked tables state they are top-N and carry an `other` row for
   the remainder, as `traces.summary` already does with `other_ms`.
2. **What to do next** — §9.6's remedies and the noise-floor verdict.
3. **Per-snapshot detail** — with **every table declaring its tier provenance**
   (exact / estimated / sampled), not only the legend. Units in every column header.
4. **The full per-span-name vocabulary** — not the top rows. A reader comparing two
   documents months apart needs to see names appear and disappear; that is the signal
   the code moved underneath and the comparison is no longer apples to apples.
5. **Definition and how to read the numbers** — the four categories, last, because it is
   reference material. A one-line legend precedes the first table so a reader who stops
   early is not left guessing.

### 9.6 What the document must admit — with remedies

| Limitation | What to do |
|---|---|
| `ingest.drops_in_window > 0` | Spans were lost **before the collector**. `matched` under-counts and every tier is affected. Reduce load, raise the channel, or re-run — no number here is safe. |
| Arm truncated (`complete: false`) | Self time and paths describe a **mixture**, not the prefix's workload. Raise `max_sample_bytes` or narrow the filter, re-run. |
| `nesting: "undetected"` | Below `tree`, cumulative may double-count and nothing can tell you. Re-run at `tree` to quantify. |
| `nested_matches > 0` | Cumulative double-counts. Quote `self_ms`, or quote cumulative as *total work including nesting*. |
| Unmatched descendants of matched spans | Their time is **silently attributed to the nearest matched ancestor** — it reads as that ancestor doing the work. Broaden the filter, or read self time as self-plus-unmatched-descendants. |
| `overlapping_child_ms > 0` | The tree is not properly nested (spawned tasks, clock skew). Self time is clamped; treat the overflow as unattributed. |
| Percentiles `estimated` | ±`error_pct` per measurement, wider on a **delta**. Differences inside it are no difference. |
| Single-run arm | Floor unknown — repeat before concluding. |
| Warm-up excluded | By how much; `exact` and `estimated` are both null. |
| Cumulative under N-way parallelism | Total work, not elapsed time. |
| Spans outside the filter | Absent entirely. |
| Several services | Cross-process clock skew reaches wall-union and path timings. |
| **Time, not correctness** | A faster arm may be faster because it does less, or because it is wrong. This measurement cannot tell you. Check the suite's own pass/fail. |
| Retried OTLP batches | No `span_id` dedup: a re-delivered batch inflates `count` and subtracts a child twice. |

Months later these are caveats; **now they are instructions**, and each is cheap to act
on now and impossible to act on later.

**Arms measured back-to-back are confounded with time** — thermal drift, background load,
page-cache state. The document shows arms in chronological order and recommends
**interleaved A/B/A/B** ordering.

### 9.7 Sizing

The readable document stays in the tens of KB; bulk data — raw sketch buckets and their
layout, per-trace tables, any raw sample prefix — goes to a sidecar named in
front-matter. The consumer loads the document into a limited context, so
self-containment is worth less than being readable at all.

### 9.8 Formats

`md` (default), `json` (structured data alone), and `folded` per snapshot.

**Folded output, corrected against the actual consumers.** Emitting **self time per
path** is right — in `stackcollapse` output each line is the exclusive weight of that
exact stack, and viewers derive a frame's width by summing lines with that prefix. Root-
first ordering is right. Three things rev 1 left open, each a silent failure:

- **Integer weight, single ASCII space.** speedscope's importer matches `^(.*) (\d+)$`
  and **drops non-matching lines with no error**, while flamegraph.pl accepts decimals.
  Decimal microseconds would render correctly in one tool and silently vanish in the
  other. Emit truncated integer microseconds.
- **Escape `;` in span names.** OTel names are arbitrary; neither consumer defines an
  escape, so `parse;render` silently becomes two frames. Replace with `,`.
- **Declare the unit.** The sidecar carries the invocation
  (`flamegraph.pl --countname us --nametype Span`), since collapsed-stack format has no
  unit concept.

`[?]` incomplete-path prefixes render as a literal root frame; the how-to-read section
says so.

### 9.9 Who writes the file

**The daemon returns bytes; the client writes.** rev 1 cited `logs.export` as a
file-writing precedent — it writes nothing; the writes are in the MCP server and CLI.
That is the right division: the broker runs as a service, so a relative path would
resolve against the daemon's cwd and user rather than the caller's. `path` is a
client-side concern, and the sidecar is written beside it by the same client.

---

## 10. Lifecycle, persistence, restart

- **Definitions persist** for named sessions, as triggers/filters/bookmarks do.
- **Snapshots persist write-through, on `collectors.snapshot`** — not only at shutdown.
  rev 1 claimed a run "stops being at risk the moment it is snapshotted" while
  `save_state` was called only on graceful shutdown, so a crash or OOM lost everything
  since boot. That composes badly: a profiling run with a 256 MiB sample budget on top
  of the existing rings is the workload **most likely to OOM the daemon**, which is
  exactly the case that lost the data.
- **Snapshots live in per-collector files, not `state.json`.** `state.json` is written
  with `to_string_pretty` and a non-atomic `std::fs::write`; a multi-MB sketch blob
  rewritten on every save risks a truncated file taking triggers, filters, and bookmarks
  with it. Snapshot files are written temp-then-rename.
- **Sketch layout identity is persisted with the buckets**, so a snapshot written before
  an upgrade is never read under a newer default layout (§3.5, §5.6).
- On restore a collector comes back **armed but zeroed**, carrying `zeroed_by:
  "daemon_restart"`; its history is unaffected.
- **A collector whose pinned domain no longer exists restores `orphaned`, not armed.**
  Ephemeral domains are not re-created at boot, and `PersistedSession` does not record a
  domain binding — so rev 1's restore path would have produced a collector that reads as
  armed and collects nothing forever, reintroducing the exact failure domain pinning
  exists to prevent. Restoring a pinned domain requires recording it; where that is not
  possible the collector is orphaned loudly.
- **Session TTL disposal takes collectors and their history.** Agent sessions disconnect
  constantly and the sweep disposes past `session_ttl_secs` (default 24 h). Snapshots
  persist independently of the session, and disposal is reported.
- Duplicate collector name within a session → error.

---

## 11. Error handling

Loud and specific; never a zero, an empty result, or a silent fallback.

| Condition | Response |
|---|---|
| Metric above the collector's level | Error naming the level |
| `group_by: "attr:x"`, `x` not a group key | Error listing available keys |
| Filter contains a log-only qualifier | Error (new `is_log_filter`, §7) |
| Bookmark qualifier in a collector filter | Rejected, matching `filters.add`/`triggers.add` |
| Arming exceeds the daemon reservation | Error stating requested, remaining, and the four-collector default ceiling |
| Snapshot policy above the level | Error at `add`, not at snapshot time |
| Reading a snapshot for an excluded projection | Error naming the excluding policy |
| Unknown snapshot label | Error listing available labels |
| `edit` changing filter with history, or lowering level | Refused (§7.1) |
| `diff` across sketch layouts | Refused — arithmetically wrong |
| `diff` level or filter mismatch | **Marked**, not refused (§5.6) |
| `diff` incomplete side | Sample rows null-with-reason; exact and estimated pass |
| Group-key cardinality cap | `__overflow__` + `cardinality_capped: true` |
| Per-name sketch cap | `__overflow__` + `name_sketches_capped: true` |
| Sample budget hit | `sampled.complete = false`; exact tier and sketch unaffected |
| Ingest drops in window | `ingest.drops_in_window > 0`; `trustworthy: false` |
| `nested_matches == 0` at `tree` | `self_ms` suppressed with a reason (§5.3) |
| Malformed timestamps | Counted, excluded from sketch/union/warm-up origin |
| `document` folded on non-`tree` | Error naming the level |
| `document` with no snapshots | Valid; states history is empty |
| `document` without question/finding | Valid; recorded as `null`; `trustworthy` not asserted |
| Zero matches | Well-formed empty result |

---

## 12. Test list

**Verification**

- **V1** Per-match memory re-derived from `size_of` matches §3.2; sketch memory measured,
  not assumed.
- **V2** Exact scalars stay exact across the cap boundary.
- **V3** `sampled` percentiles match a hand-computed nearest-rank reference (§5.7).
- **V4** Self time on a hand-built three-level tree.
- **V4b** **Child outliving its parent** → self clamped at zero, overflow in
  `overlapping_child_ms`. V4 alone cannot catch this.
- **V5** Wall union over nested, overlapping, disjoint intervals.
- **V6** Path aggregation; `[?]` + `path_incomplete` when an ancestor is excluded.
- **V7** `group_by` name / trace / attr, including **boolean and numeric** attributes and
  the `__absent__` bucket (§3.3).
- **V8** `skip_warmup_ms` origin is `min(start_ns)`; both `exact` and `estimated` null.
- **V9** Diff deltas per percentile, with the **delta** error bound (§5.6).
- **V10** `reset` returns and zeroes atomically; `wall_ms` measured from `zeroed_at`.
- **V11** Restart → armed, zeroed, history intact — **including a `kill -9` variant**,
  since the harness's graceful `restart()` would pass regardless.
- **V12** `traces.slow` aggregates the full matching population; `min_duration_ms`
  applied only as a display floor; nearest-rank per §5.7.
- **V13** Sketch percentiles within `error_pct` of sample-exact on uniform, bimodal, and
  heavy-tailed distributions, at a stated minimum n and matched rank convention.
- **V14** Snapshot records/returns/zeroes; `reset` discards; history matches.
- **V15** Each snapshot echoes its own policy, filter, level, and layout; editing does
  not alter existing snapshots.
- **V16** Merge equals the sum of its parts; sample-derived projections omitted with a
  reason; **mismatched layouts refused**.
- **V17** Descriptions and `meta` appear in read, diff, history, document.
- **V18** Document carries everything §9.4–§9.8 requires, per-arm provenance included.
- **V19** Noise floor propagated into every delta row; single-run states unknown.
- **V20** Document within its size target; bulk in the sidecar.
- **V21** Omitting `path` returns bytes; the client writes identical bytes.
- **V22** Regeneration is lossless.
- **V23** Body order per §9.5, and each table declares its tier provenance.
- **V24** `is_log_filter` admits bare patterns, attribute-only filters, and bookmark
  windows as span filters, and rejects log-only qualifiers.

**Adversarial**

- **A1** Cap boundary: one under / at / one over.
- **A2** Parent cycle → depth cap terminates; no hang.
- **A3** Arrival order, **complete case**: identical projections.
- **A3b** Arrival order, **truncated case**: exact tier and sketch identical across two
  orderings; sample-derived fields permitted to differ. rev 1's A3 pinned the claim only
  where it was trivially true.
- **A4** Concurrent reset during ingest: no lost update, no torn read.
- **A5** Domain rebinding mid-collection: collector keeps its pinned domain.
- **A6** High-cardinality group key → `__overflow__`, bounded.
- **A6b** High-cardinality **span names** → per-name sketch cap holds (§3.5).
- **A7** Diff level mismatch **compares at `min(level)`**; filter mismatch is marked;
  layout mismatch is refused.
- **A8** Incomplete diff: sample rows null, exact and estimated pass; `scalar` permitted.
- **A9** Idle CPU zero with a collector and a threshold armed.
- **A10** Truncation must not launder into a snapshot: sketch percentiles still whole-run;
  sample-derived fields carry `complete: false` through.
- **A11** Snapshot during ingest: `snapshot.count + live.count` equals total ingested.
- **A12** History eviction: labels never reused; survivors intact.
- **A13** `edit` permitted on a virgin collector, refused with history; level raise
  permitted, lower refused.
- **A14** **Observer effect, redesigned.** rev 1 ran the suite with and without collectors
  and compared wall time — structurally unable to detect anything, because ingest is a
  detached task behind a 65 536-slot `try_send` channel with no backpressure onto the
  system under test. Overhead could be 10× with no wall-time signal, until the channel
  overflows and the symptom becomes *dropped spans*, which rev 1 did not measure. Now:
  1. a **criterion bench** over `process_span_for_domain` with 0/1/4 collectors armed —
     **there is no bench infrastructure in the repo, so this is a phase-1 dependency**;
  2. a **saturation test** asserting `ReceiverDropSnapshot` stays at zero with collectors
     armed at a rate that is clean without them;
  3. a **read-during-ingest test**: fill to the cap, run `collectors.get(group_by:
     "path")` while ingesting, assert zero drops — the §3.6 lock discipline;
  4. stated repetition count and pass threshold, per the spec's own §6.5 rule.

---

## 13. Build order

Each phase ends with targeted tests green and a fresh-eyes self-review. Mid-checkpoint
after phase 1.

1. **Core.** Levels, chunked sample tier with the §3.6 lock discipline, exact tier with
   integer-nanosecond accumulation, ingest-fed sketch, ingest-drop accounting,
   domain-keyed registry, `add`/`list`/`get`/`reset`/`remove`, `traces.profile`,
   `is_log_filter`, the §4.2 matcher de-allocation, bench infrastructure, and the §1.1
   `traces.slow` fix.
   **Phase 1 ships the complete `ProfileResult` field set**, with later-phase fields
   present and inert — `exact` nullable, `exact_unavailable_reason`, `cardinality_capped`,
   `path_incomplete`, `estimated_unavailable_reason`. Adding them later is a wire break
   on a shipped type.
2. **Projections.** Path aggregation, wall union, warm-up exclusion, `group_keys` with
   §3.3 value rendering.
3. **History.** Snapshot / history / edit, policy validation, write-through persistence
   in per-collector files, merging.
4. **Comparison.** `collectors.diff` with §5.6 marking; `collectors.document`.
5. **Guards.** Threshold triggers, built from scratch (§8).

Phases 1–3 are the complete driving workflow; 4–5 cut cleanly.

Docs are part of done: README tool table and profiling section, the logmon skill's
selection table, and deletion of the stale span-trigger roadmap line. Note the MCP
surface grows from 33 tools to 44, roughly doubling per-session tool-schema context cost
— worth stating in the skill.

**Deferred:** log-derived durations; cross-collector baselines; multi-key `group_by`;
reservoir sampling as an opt-in percentile-only mode; `span_id` dedup for retried
batches.

---

## 14. Design gate — round 1 outcome

Four lenses run against `709651b`: buildability, soundness + false-positive,
performance + prior-art, and cold-reader. Findings resolved throughout this revision and
logged at the head of this document.

Two lens choices proved their worth and should be repeated on any similar design:

- The **cold reader** — given only a synthetic document, explicitly barred from the spec
  and the codebase — produced findings no spec-reading lens could, because §9's central
  claim is that the document is interpretable without context, and that is only testable
  by trying it. It also independently derived an invariant the spec never stated.
- Naming a **weak claim** for the gate to attack worked: §4.3's order-independence was
  flagged as load-bearing and unproven, and came back false.

## 15. Re-gate before build

A substantive change made in response to a finding re-arms the gate. These mechanisms are
new or materially different in rev 2 and were **not** reviewed by round 1:

- §3.6 chunked sample tier and its lock discipline — the fix for the most dangerous
  finding, and itself concurrency-critical.
- §3.5 sketch choice, unit, range, layout identity, and budgets.
- §5.1's four-category shape, and cumulative/self as peers.
- §5.3's clamp-and-account rule for negative self time.
- §5.6/§7.1's re-scoped checks — a false-positive lens should confirm the *new* boundaries
  do not swing to false negatives.
- §10's write-through persistence and per-collector files.

Round 2 should also carry a **fresh cold-reader pass** on a document regenerated under
the rev-2 shape, since every finding that lens produced changed the artefact it read.
