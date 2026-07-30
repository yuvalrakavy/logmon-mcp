# Span Time Collector — Design Spec

**Date:** 2026-07-29 (rev 3 — post design-gate round 2)
**Status:** DESIGN — two gate rounds, eight fresh-context lenses. Rev 3 resolves round 2.
Mechanisms changed in rev 3 need a round-3 pass on a reduced lens set (§16).
**Tier:** **T2** — novel feature minting externally-binding contract surface.

> ## Gate log
>
> **Round 1** (buildability, soundness+false-positive, performance+prior-art, cold reader)
> confirmed the design's *structure* and destroyed several claims made about it: `sum_ms`
> double-counting nesting, `matches_pattern` allocating, `group_keys` failing on non-string
> attributes, the exact tier not being exact, and order-independence being false under
> truncation.
>
> **Round 2** (concurrency+durability, false-negative, prior-art+numbers, cold reader) found
> **every one of rev 2's three headline fixes individually broken**, all four of its
> loosenings holed, and three of its new rules wrong. The lesson is general and is recorded
> here because it outlives this feature: **fixes written under gate pressure are less
> grounded than the design they repair.** Rev 1's failures were claims about existing code;
> rev 2's were claims about mechanisms invented in response to findings. That is exactly why
> a substantive change made in response to a finding re-arms the gate.
>
> Round-2 headlines, all resolved below:
> - **Ingest accounting was blind to the losses it existed to detect.** The HTTP path sheds
>   whole batches at 429 *before* any counter moves, and malformed spans land in a counter
>   that is never read (gRPC) or does not exist (HTTP). §5.1's fix required new plumbing,
>   not a new field. (Verified: `http.rs:450`, `grpc.rs:288-300`, `malformed_count` has no
>   reader.)
> - **The chunked sample tier had a whole-chunk race.** Sealed list and active chunk were two
>   lock domains; a reader could miss or double-count 65 536 records. Replaced with a
>   single lock over both domains, with only destructive reads swapping (§3.6). A probe
>   then showed rev 3's first attempt — swap-and-fold on *every* read — was itself wrong.
>   Corrected in place; see §3.6.
> - **Write-through persistence did not compose.** Definitions were still shutdown-only, so a
>   `kill -9` left orphan history with nothing to attach it to and V11 failed by
>   construction. And because `domains.create(persist=true)` is refused today, orphaning was
>   the *primary* path, not the exception (§10).
> - **Clamp-and-account solved the wrong problem.** The Σ was the defect, not the negativity:
>   a 100 ms parent with two concurrent 60 ms children has self time 40 ms, and the clamp
>   returned 0 + an uninterpretable residue. Elastic APM's clipped interval union is the
>   prior art, and §5.2's merged-interval routine was already in the design at the wrong
>   scope (§5.3).
> - **The percentile rank convention was incompatible with the chosen library**, voiding the
>   accuracy bound it quoted, and V13 was unsatisfiable as written (§5.7).
> - **The delta error bound was wrong by an order of magnitude, in the same direction as
>   rev 1** — one paragraph after naming that failure mode (§5.6).
> - Four loosened checks each opened a hole; a level raise could falsify `complete` itself.
>
> Round 2 also **corrected round 2**: one lens claimed `ReceiverMetrics` is global, another
> proved it per-domain. Verified per-domain (`domain_lifecycle.rs:57`, `domain.rs:127`).
> Agent findings are hypotheses.

---

## 0. Motivation

logmon answers *"what happened?"*, not *"where did the time go, and did my change help?"*.

Driving case: measure a cache behind a kill-switch by running a suite twice and comparing
time in a class of spans — the **capability A/B** required of any kill-switched optimisation,
which logmon cannot support today.

**Collect broadly; decide at read time.** The collector computes no metrics; it retains a
compact record per matched span and every metric is a read-time projection.

---

## 1. Seams — verified at `a08c119`

| Seam | Status |
|---|---|
| `process_span_for_domain` ingest hook | **Confirmed.** Span triggers exist; the README roadmap line saying otherwise is stale and is deleted here. |
| `matches_span` | **Confirmed with caveats.** Attributes are **string-only** (`.as_str()`), and matching **allocates** (§4.2). |
| Bookmark→`SeqFilter` on spans | **Confirmed.** Used by `traces.profile` only (§7). |
| `SessionRegistry` | Collectors follow the triggers/filters pattern for lifecycle, **but the ingest index is domain-keyed** (§4.4). |
| `ReceiverMetrics` | **Per-domain** (`domain_lifecycle.rs:57`). Four of six counters are *log* drops; only `otlp_http_traces` / `otlp_grpc_traces` can represent a lost span. |
| Single-writer ingest | **Confirmed and load-bearing.** One span-processor task per domain (`span_processor.rs:21`, `domain_lifecycle.rs:106`). No writer–writer races exist; every lock below is uncontended on the hot path. |
| `SpanStore` ring, 10 000 default | Confirmed; eviction is silent. |
| ~~Per-trigger debounce~~ | **Retracted (rev 2).** The cited symbol is the session storage window; the real counter measures *entries*; span triggers are not debounced at all. §8 reuses nothing. |
| ~~`logs.export` file pattern~~ | **Retracted (rev 2).** The RPC writes nothing; writes are client-side (§9.9). |

### 1.1 Existing defects touched

- **`traces.slow` grouped arm aggregates a biased sample** — `slow_spans` filters by
  `min_duration_ms`, sorts, `truncate(count)`, *then* the handler groups. **Both** truncations
  bias it. Fixed: aggregate the full matching population; `min_duration_ms` becomes a display
  floor applied after aggregation. Its rank convention must match §5.7 (today
  `floor(n·0.95)` returns the maximum at n=20). **Fold the aggregate inside the read guard** —
  the current code deep-clones every match into a `Vec<SpanEntry>` under the lock, and removing
  the truncation grows that clone set to the whole buffer while `insert` needs a write lock.
- **Trigger path re-parses and allocates per span**, including a regex compile per span per
  bound session. Filed separately, then **pulled into phase 1 and fixed** once the bench showed
  it makes the exact tier unachievable in practice (§4.2).
- **OTLP/gRPC reports success while dropping spans mid-batch.** Filed separately; §5.1 must
  cope with it either way.

---

## 2. Scope

In: collector lifecycle; two-tier retention (exact tier with integer-nanosecond scalars and a
duration sketch, plus a chunked columnar sample tier under one lock); levels; `group_keys`;
read-time projections including total and self time, percentiles, wall union, nesting
detection, and call paths; **ingest-loss accounting**; snapshot history with merging;
descriptions and `meta`; `traces.profile`; `collectors.diff`; `collectors.document`; threshold
triggers (phase 5); the §1.1 `traces.slow` fix.

Out: re-importing a document (§9.1); cross-collector baselines; log-derived durations;
multi-key `group_by`; reservoir sampling (§3.4); `span_id` dedup for retried batches (§9.6
row instead); changes to how spans are received.

---

## 3. Data model

### 3.1 Exact tier

Per collector, per span name, per group: `count`, `total_ns` (`i128`), `min_ns`, `max_ns`,
`error_count`, and a duration sketch (§3.5).

**`i128`, not `u64`.** Durations can be negative — `duration_ms` is
`(end_nano − start_nano) as f64 / 1e6` on both transports, and §5.3 expects child-outlives-parent
and clock skew. A `u64` accumulator underflows: panic in debug, wrap in release. Accumulate
from `end_time − start_time` in nanoseconds directly, **not** from `duration_ms`, or the
integer argument buys nothing.

`total_ns` sums every matched span and **counts a matched span nested inside another matched
span at both levels**. That is the quantity's definition, not a defect, and §5.1 names it.

### 3.2 Levels

| Level | Columns added | Unlocks | Bytes/match |
|---|---|---|---|
| `scalar` | *(exact tier only)* | count, total, avg, min, max, error count, estimated percentiles | 0 |
| `timing` | `start_ns i64`, `end_ns i64`, `name_id u32`, `flags u8` | sample-exact percentiles, wall union, warm-up exclusion | 21 |
| `tree` **(default)** | + `span_id u64`, `parent_span_id u64`, `trace_id u128` | **self time**, nesting detection, per-trace rollups, call paths, folded output | 53 |

Plus 4 B/match per `group_keys` entry at `timing`+. The exact tier's key is widened by
`group_keys` **at every level**, so `groups[].exact` is exact everywhere.

`parent_span_id == 0` means root (`bytes_to_span_id(&[0;8]) → None`, so 0 is never a real id).
**This is why a level raise may not zero-fill** (§7.1).

Below `tree`, the nesting question cannot be asked: results carry `nesting: "unknown"` and
never print a bare total that could be mistaken for work done. (`"undetected"` is reserved
for a `tree`-level run that looked and found none — the two are different claims, and only
the first is a reason to re-run.)

### 3.3 `group_keys`

Attribute names interned per collector. Values are read **independently of the matcher**,
because the matcher's `.as_str()` makes a boolean `cache.enabled` — the driving example's
likely encoding — invisible. Rendering: `Bool` → `"true"`/`"false"`; `Number` → shortest
round-trip decimal; `String` → itself; `Array`/`Object` → excluded with a reason. A span
missing the key falls into `__absent__`.

Per-key cardinality cap 1024 → `__overflow__` + `cardinality_capped: true`.

**`__overflow__` is a different population on each side of a comparison** — which values land
in it is arrival order — so it is **suppressed in `collectors.diff`**, never compared (§5.6).
`__absent__` has identical semantics on both sides and compares fine.

**Caps reset with the window.** The interner and both cardinality counters are part of the
state swapped on a destructive read (§3.6), so they are per-window, not lifetime. Otherwise the recommended
interleaved A/B/A/B workflow would exhaust a cumulative cap and pin `cardinality_capped` on
for the rest of the run.

### 3.4 Sample budget

`max_sample_bytes` default 64 MiB (≈1.27 M records at `tree`); daemon-wide
`max_total_sample_bytes` 256 MiB, enforced as a **reservation** at arm time — so four
default-sized collectors is the practical ceiling, and `collectors.add` says so when refusing.

Chunks are **record-aligned groups spanning all columns**, not one chunk per column, so
allocated capacity exceeds payload by at most one partial chunk. Chunks seal only when full;
a read must never seal (§3.6), or N reads produce N partial chunks and the bound diverges.

**`raw_sample_bytes` snapshot prefixes count against the reservation**, at `max_snapshots ×
raw_sample_bytes` — otherwise 50 retained prefixes sit outside the budget whose entire purpose
is preventing the daemon OOM that §10 is written around.

On reaching the budget the sample tier stops retaining — prefix truncation, cold-biased,
marked `complete: false`. **Reservoir sampling is rejected**: it breaks self time and wall
union, which need completeness.

### 3.5 The duration sketch

**DDSketch** (`sketches-ddsketch >= 0.4`), relative accuracy α = 0.01.

*Why, stated correctly this time:* it is **occupancy-driven rather than range-driven**, so
memory scales with what was measured rather than with the declared range. Rev 2 claimed a
sparse map and an order-of-magnitude advantage; the crate is a **contiguous `Vec` between the
lowest and highest occupied key**, and at the like-for-like setting (sigfig 2, `u32` counters)
HdrHistogram is ~18 KiB/name against DDSketch's ~2.7 KiB narrow / ~11.3 KiB fully-occupied.
The gap is ~1.5–6×, not 10×. HdrHistogram remains a viable fallback; DDSketch wins on
occupancy, a published proof, and a cross-language binary format.

**Input: integer nanoseconds. Declared range 1 ns – 1 h, and the range is *enforced*** —
inputs are clamped to it before `add()` and out-of-range values counted separately, and
`max_num_bins` is set explicitly rather than inherited. DDSketch has no upper bound of its own;
its only ceiling is `bin_limit` (default 2048), and exceeding it **collapses the low end** —
silently doing the exact thing nanosecond input was chosen to prevent.

**Zero and negative durations are handled explicitly, independent of the malformed-timestamp
path.** A legitimate `start == end` span (a cache hit, or anything below clock resolution) is
not malformed; it is counted, recorded in the sketch's zero bucket, and reported as `0.0`. A
**negative** duration is excluded from the sketch entirely — the crate would route it to a
negative store where it participates in `quantile()`, and a negative p50 is not a
percentile anyone wants — and counted as `negative_duration_spans`.

**No byte budget.** Rev 2's `max_histogram_bytes` was derived from nothing, was 7–45× larger
than the cap it guarded, and had **undefined exhaustion behaviour** — which falsified §5.6's
premise that `estimated` always covers the whole run. Removed. Sketch memory is bounded by
`max_name_sketches` (256) and `max_group_sketches` (64), both with `__overflow__` treatment,
and the derived total is reported by `collectors.add`.

**Layout identity is logmon's bookkeeping**, recorded at construction as
`{algo, alpha, unit, range, max_num_bins}` — `Config::min_value` is `pub(crate)` and cannot be
read back. §5.6's only hard block depends on that record being trustworthy.

**Persist via `to_java_bytes`/`from_java_bytes`** (0.4+), never the serde derive, whose fields
are `pub(crate)` and whose layout changed between 0.2 and 0.4 — persisting it would make the
on-disk format hostage to a 0.x crate's private internals.

**Axis restriction.** Sketches exist per collector, per name, per group. Any projection off
those axes — `group_by: "path"` or `"trace"`, a duration band, read-time narrowing on anything
else — has no sketch and falls back to `sampled`, with `estimated: null` and a reason.

### 3.6 One lock, and only destructive reads swap

Rev 2's chunked tier split the sealed list and the active chunk into two lock domains, which
loses or duplicates a whole 65 536-record chunk depending on read ordering. Replaced.

**All collector state lives under ONE lock** — the defect in rev 2 was the lock *scope*, not
the chunked structure:

1. sealed chunks `Vec<Arc<Chunk>>`, immutable once sealed · the one active mutable chunk
2. collector exact tier · 3. per-name exact map · 4. per-group exact map
5. collector sketch · per-name sketches · per-group sketches
6. the interner and both cardinality counters
7. ingest baseline (`ReceiverDropSnapshot`) and `malformed_timestamps`
8. `armed_at` / `zeroed_at`, `complete`, sample-byte accounting
9. `__overflow__` / `name_sketches_capped` flags

**Ingest** takes that lock once per matched span and updates every structure inside it.
Uncontended, because ingest is single-writer per domain (§1).

**Non-destructive reads clone; only destructive ones swap.**

- `collectors.get` and `snapshot(reset=false)`: under the lock, clone the sealed `Arc` list
  (pointer copies) and **copy the active chunk**, clone the exact tier and sketches; release;
  project outside. Cost is O(active chunk), not O(N).
- `snapshot(reset=true)` and `reset`: swap the whole structure out under the lock — O(1) —
  and project outside it.

> **Probe evidence (measured 2026-07-29, this machine, synthetic 2 M-span workload —
> `scratchpad/gen-probe`).** Rev 3 originally specified *every* read as a swap-and-fold into
> a retired accumulator. **That is wrong for non-destructive reads, and the probe proves it.**
> Exact scalars and sketches are additive, but **self time and wall union are not**: a parent
> separated from its children by a generation boundary keeps their time, so the fold
> over-reports. Measured **7.98 s of phantom self time** across 67 split parent/child groups
> over 100 reads at 200 ms parents. The disqualifying property is not the magnitude but the
> shape — **the error scales with how often the collector is read**, so reading it would
> change its reported self time. Chunk-cloning is exact in the same test.
>
> **Chunk size 8 192, not 65 536**, also measured: worst read lock-hold **0.29 ms** at 8 192
> versus **1.80 ms** at 65 536, with ingest throughput under concurrent readers 19.0 M vs
> 4.3 M spans/s. Both figures are from this probe on this machine and must be re-measured on
> the real record layout (§12, V1).
>
> The probe ran 341 concurrent reads against one writer with **zero invariant violations** —
> no missing or duplicated records, and the exact tier agreeing with the retained records on
> every read. That is the A11 invariant, demonstrated rather than argued.

There is deliberately **no retired accumulator**. After `snapshot(reset=true)` the
sample-derived projections restart, which is correct: the snapshot took that data.

Window stamps, the drop baseline, and the `snapshot-<n>` counter increment happen **inside**
the swap, or they describe a window that does not match the data printed beside them.

**File I/O never happens under the lock**, and never on the runtime — serialize and write
outside the critical section, on `spawn_blocking` (`RpcHandler::handle` is synchronous and
the crate uses no `spawn_blocking` today).

---

## 4. Ingest path

### 4.1 Hook

A third step in `process_span_for_domain`: evaluate this domain's collectors.

### 4.2 Cost — corrected twice

**Matching allocates.** `matches_pattern` is
`text.to_lowercase().contains(&s.to_lowercase())` — two heap allocations per pattern per span;
`sk=` adds a `format!`. **In scope for phase 1:** lowercase in `Pattern`'s `Deserialize` (the
DSL path already lowercases) and make the matcher use a non-allocating case-insensitive
comparison. This is shared with the log path — a deliberate shared-code change.

Collector hot-path work: pre-parsed filter; one domain-keyed registry read; one collector
lock; chunk append; sketch `add`; interner hash lookup.

**The sketch's allocation shape, accurately:** a key arriving *below* `min_key` triggers
`bins.rotate_right(shift)` — a memmove over the whole occupied span, inside the lock. Bounded
and self-limiting (log-scaled, a few hundred times per run at most), but biased toward the
start of a run and toward *fast* spans, i.e. the cache-on arm.

**Existing path baseline for A14 — MEASURED**, not derived (criterion, `crates/core/benches/
span_ingest.rs`, this machine, release):

| Case | Per span | Implied ceiling |
|---|---|---|
| `process_span`, 0 sessions bound | **3.94 µs** | ~254 k spans/s |
| 1 session bound | **34.6 µs** | **~29 k spans/s** |
| 4 sessions bound | **118.6 µs** | ~8.4 k spans/s |

**92 % of the per-session cost is one regex, compiled per span.** Attributed rather than
inferred: `parse_filter("/panic|unwrap failed|stack backtrace/")` — the trigger
`SessionRegistry` seeds into *every* session — measures **28.07 µs**, against 140 ns for the
other seeded filter and 263 ns for a collector-shaped `sv=store_server, d>=10`.
`is_span_filter_str` calls `parse_filter` on every trigger for every span, so that compile is
paid per span per bound session. The rest is the deep `SpanEntry` clone under a `SpanStore`
**write** lock and lock growth of `2 + N_registry + 2×S_bound`.

**This reprioritises §4.2's own contents.** The matcher de-allocation — flagged by three
convergent round-1 lenses — is real and confirmed (`matches_span` with a substring qualifier
is **66 ns** against a **4.0 ns** allocation-free duration-only control, so ~94 % of a
substring match is the two `to_lowercase` calls) — but it is **0.2 % of the per-session ingest
cost**. Convergent lens agreement identified a true defect and said nothing about its
magnitude; only the bench could.

**Consequence, and it is not cosmetic.** A 16-lane suite emitting faster than ~29 k spans/s
fills the 65 536-slot channel in seconds, and spans are then dropped *at the receiver*, before
any collector sees them (§5.1.1) — so every collector result would carry `trustworthy: false`
for a reason having nothing to do with collectors. The exact tier's central claim was
unachievable in practice while that stood, which made the trigger-path fix a **prerequisite**
of this feature rather than the adjacent cleanup it had been filed as.

**Fixed in phase 1. After, measured on the same bench:**

| Case | Before | After | |
|---|---|---|---|
| 0 sessions | 3.94 µs | 3.91 µs | unchanged — no trigger work on this path |
| 1 session | 34.6 µs | **4.35 µs** | −87.7 %, ~29 k → ~230 k spans/s |
| 4 sessions | 118.6 µs | **4.66 µs** | −96.1 %, ~8.4 k → ~215 k spans/s |

The shape matters as much as the magnitude: per-span cost is now nearly **flat in session
count** (3.91 / 4.35 / 4.66 µs) instead of growing ~30 µs per bound session.

`TriggerManager::evaluate_span` matches against each trigger's already-stored `ParsedFilter`
under one lock, mirroring the log path's `evaluate`. No parse, no regex recompile, and no
`TriggerInfo` materialisation — `collect` into an empty `Vec` does not allocate, so the
no-match case (the overwhelmingly common one) allocates nothing at all. Behaviour is
deliberately unchanged: span triggers are still **not** debounced, which is now pinned by a
test rather than by a comment.

### 4.3 Ordering, clocks, malformed input

**Order-independence holds for values, not for the key set.** Integer-nanosecond accumulation
is associative; sketch insertion is commutative; so the exact tier and sketch are
order-independent *in arithmetic*. But **which** keys survive the §3.3 cardinality caps depends
on arrival order, and that reaches per-group exact rows and per-name sketches. And under prefix
truncation, which records are retained is arrival order. So:

> Arrival order does not affect any result while `sampled.complete` is `true` **and** no
> cardinality cap has bound.

`start_ns`/`end_ns` are producer clock; the collector window is broker clock; warm-up is
defined against `min(start_ns)` (§5.5) so the two never mix.

**Malformed timestamps** (the HTTP path defaults missing values to 0) are counted, and excluded
from the sketch, wall union, and warm-up origin. **Negative durations are excluded from the
sketch and from `total_ns`**, and counted separately — they are a different population from
malformed timestamps and rev 2 conflated them.

### 4.4 The registry is domain-keyed

A collector is pinned to its creation domain and reached through a **domain-keyed registry**,
independent of `SessionState::domain`. If collectors were reached the way triggers are — via
`active_session_ids_for_domain` — then `use_domain` mid-run would silently stop the collector
while it still reported its pinned domain: the precise failure pinning exists to prevent.

Two consequences to state rather than discover:

- **Named sessions keep ingesting while disconnected.** `active_session_ids_for_domain` does
  not filter on `connected`, and the driving workflow (arm → disconnect → run suite →
  reconnect → read) depends on it. Today accidental; pinned by a test.
- **Anonymous sessions are removed on disconnect**, so `collectors.add` on one refuses (§11).

---

## 5. Read-time projections

### 5.1 Result shape

```jsonc
{
  "collector": "cache-ab", "description": "…",
  "filter": "sv=store_server", "level": "tree",
  "matched": 1180442,
  "nesting": "detected",             // "detected" | "undetected"
  "window": { "armed_at": "…", "zeroed_at": "…", "read_at": "…", "wall_ms": 2540000 },

  "ingest": {                        // null when the pinned domain is gone
    "drops_in_window": 0,            // otlp_http_traces + otlp_grpc_traces only
    "shed_batches": 0,               // NEW plumbing (§5.1.1)
    "malformed_dropped": 0,          // NEW plumbing
    "malformed_timestamps": 0, "negative_duration_spans": 0,
    "attribution": "domain"          // not per-collector; see below
  },

  "exact": {                         // null under warm-up exclusion (§5.5)
    "count": 1180442, "total_ms": 1234567.8,
    "avg_ms": 1.046, "min_ms": 0.01, "max_ms": 4310.2, "error_count": 12
  },
  "estimated": { "axis": "collector", "alpha_pct": 1.0,
                 "p50_ms": 0.30, "p80_ms": 1.10, "p95_ms": 4.20, "p99_ms": 22.0 },
  "sampled": {
    "complete": true, "sample_count": 1180442,
    "self_ms": 736201.4, "nested_matches": 412880,
    "overlapping_child_ms": 3118.4, "overlapping_child_spans": 214,
    "wall_union_ms": 310221.0, "achieved_concurrency": 3.98,
    "p50_ms": 0.30, "p80_ms": 1.11, "p95_ms": 4.18, "p99_ms": 22.1
  },
  "groups": [ { "key": "…", "exact": {…}, "estimated": {…}, "sampled": {…} } ]
}
```

**`total_ms` and `self_ms`, not `cumulative_ms`.** Rev 2 cited pprof, gprof, and
async-profiler. Only pprof was right: gprof's "cumulative seconds" is a running sum down the
sorted table, not subtree-inclusive — so the word *cumulative* actively misleads a gprof user.
`total`/`self` matches Chrome DevTools, Firefox Profiler, Pyroscope, and py-spy's sense.

**Where the profiler analogy breaks, stated because rev 2 invited the wrong prior:**

- pprof counts a location **once per sample** even under recursion; `total_ms` deliberately
  counts a matched span nested in another at both levels. The analogy *inverts* here.
- In pprof, `Σ flat` equals the profile total and `cum ≤ total`. Here spans are concurrent
  wall-clock intervals: `Σ self_ms` can exceed elapsed time under parallelism, and `total_ms`
  is unbounded above `wall_ms`. §9.6 carries this for **both** fields — rev 2 covered only
  the total, which was worse, since self is the field it told readers to prefer.

**`achieved_concurrency` = `total_ms / wall_union_ms`** — how parallel the matched work
actually was. Cheap, and it contextualises the "total work, not elapsed time" caveat with a
number instead of a warning.

#### 5.1.1 Ingest accounting needs new plumbing, not a new field

Rev 2 added `drops_in_window` as a `ReceiverMetrics` delta. That counter **cannot see the two
dominant loss paths**:

- **HTTP sheds whole batches at ≥80 % occupancy and returns 429 before any `try_send_span`
  runs**, so no counter moves. The repo's own test asserts `otlp_http_traces == 0` in that
  case. This is the *primary* load-shedding mechanism on that transport and it engages exactly
  when load is high — the variable under test in every A/B.
- **Malformed spans** increment `OtlpTraceService::malformed_count`, which is **never read
  anywhere**; on HTTP there is no counter at all (`if let Some(entry)` with no else).

So phase 1 adds `shed_batches` and `malformed_dropped` counters to `ReceiverMetrics` and wires
them. Without that, "the exact tier was never exact" is papered over, not fixed.

Three further corrections: sum **only** `otlp_http_traces` and `otlp_grpc_traces`, or a GELF
log burst flips `trustworthy: false` on a run that lost no spans; use `saturating_sub` plus an
instance-identity check, because a deleted-and-recreated domain gets a fresh counter at zero;
and hold the `Arc<ReceiverMetrics>` on the collector (captured RPC-side at arm/reset), since
`process_span_for_domain` has no metrics handle.

**Attribution is per-domain, not per-collector**, and the drops are unfiltered — most may be
spans this filter would never have matched. So §9.6 says `matched` **may** under-count. As a
conservative marking trigger it is sound; as a stated fact it would be wrong.

### 5.2 Catalogue

| Metric | Min level | Tier |
|---|---|---|
| `count`, `total_ms`, `avg_ms`, `min_ms`, `max_ms`, `error_count` | `scalar` | exact |
| `estimated.p*_ms` | `scalar` | sketch, declared axes only |
| `sampled.p*_ms`, `wall_union_ms`, `achieved_concurrency` | `timing` | sample |
| `self_ms`, `nested_matches`, `overlapping_child_*` | `tree` | sample |
| `group_by: "trace"` / `"path"` | `tree` | sample; `exact`/`estimated` null with reason |

Requesting above the level is a loud error naming the level.

### 5.3 Self time — clipped interval union, not a sum

```
self = duration − |union( clip(child_interval, parent_interval) for matched children )|
```

Rev 2 used `duration − Σ(children)` with a clamp at zero. The **Σ** was the defect. A 100 ms
parent with two concurrent 60 ms children has self time 40 ms; the sum gives −20, the clamp
returns 0 plus an uninterpretable residue — on precisely the `tokio::spawn` workload cited as
motivation. Elastic APM's `ChildDurationTimer` is the prior art: reference-counted child
intervals contribute their **union**, and a child still running at parent end is **clipped at
the parent's end**. logmon clips both ends, since OTLP permits a child to start before its
parent under skew.

The routine already exists — §5.2's `wall_union_ms` is a merged-interval computation. Rev 2
applied it at trace scope and not at parent scope. Cost is O(n log k) with children already
grouped by parent.

**Clamp-and-account survives as a narrow anomaly channel.** After union-and-clip, a negative
self time means a genuine anomaly (clock skew, a child clipped to zero width), not normal
concurrency. Those are clamped at zero, with the overflow reported as `overlapping_child_ms`
**and `overlapping_child_spans`** — one span at −500 ms and 500 spans at −1 ms want opposite
remedies and rev 2 could not distinguish them.

**The recoverable identity, stated because it makes the pair informative:**
`Σ(unclamped) = self_ms − overlapping_child_ms` exactly.

Three ways self time still lies, each in-band:

- **Filter-excluded children** — `nested_matches == 0` is definitionally equivalent to "self
  time carries no information", so `self_ms` is **suppressed** in that case with a reason.
  Unmatched *descendants* are worse than absent: their time is attributed to the nearest
  matched ancestor and reads as that ancestor's own work (§9.6 row).
- **Truncation** — a prefix is not subtree-closed, so boundary parents absorb cut children.
  A mixture, not "self time of the prefix".
- **Ingest loss** — a dropped child inflates its parent's self time with `complete` still true.

### 5.4 Call paths

`group_by: "path"` walks ancestors within the matched set and aggregates self time by path.
Depth cap 64 plus a visited set; a walk stopping at an unretained non-root parent marks `[?]`
and `path_incomplete: true`. Paths resolve only where ancestors are matched, so the idiom is a
broad filter plus read-time narrowing — which is also why §5.3's unmatched-descendant
attribution is load-bearing.

### 5.5 Warm-up exclusion

`skip_warmup_ms: N` drops samples within N ms of `min(start_ns)` over the matched set —
producer clock throughout. Sets **both** `exact: null` and `estimated: null` with reasons,
since both tiers are unwindowed.

### 5.6 `collectors.diff` — mark, block, or refuse

Deltas for `count`, `total_ms`, `self_ms`, and each percentile, overall and per group, with
the noise floor propagated per row and sub-threshold deltas suppressed **using the same
threshold that is printed** (§6.5).

**The error bound on an estimated delta, corrected.** Rev 2's ±1.4 % was √2·α — the RSS of
independent zero-mean *random* errors. DDSketch's α is a **deterministic worst-case bound on a
quantised value**; the errors do not combine in quadrature, and the relevant quantity is error
relative to the *delta*:

```
|Δ̃ − Δ| ≤ α(a + b)      ⟹      relative error on Δ ≤ α(a+b) / |a−b|
```

At α = 0.01, a 5 % change carries **±39 %**, a 1 % change **±199 %**. The clean rule, which
belongs beside the noise floor: **an estimated percentile delta is not resolvable below a ~2 %
relative change at α = 1 %.** This is a *measurement-resolution* floor, distinct from the
run-to-run floor, and it bites only `estimated.p*` rows — `count` and `total_ms` are exact, so
their deltas are exact.

| Condition | Behaviour |
|---|---|
| Different `level` | Compare at `min(level)`. **But nesting evidence is not discarded**: if *either* side reports `nested_matches > 0`, that fact attaches to the `total_ms` delta row and sets `trustworthy: false`. Rev 2 dropped `nested_matches` along with the level, reintroducing the round-1 headline defect through a different door. |
| Different `filter` | Compare canonicalized `ParsedFilter`s (lowercased substrings; `f64` thresholds by `total_cmp`). A genuine difference emits `filter_mismatch` + `allow_mismatch`. Regex spellings that compile identically will mark — acceptable, and not worth "fixing" by normalising regex sources. |
| Incomplete side | Scope to sample-derived rows; `exact` and `estimated` pass. `scalar` collectors (no sample tier) are explicitly permitted. Both sides truncated is still **refused** by default — a faster arm reaches the cap later in wall-clock terms, so the cold prefixes cover different slices. |
| **Asymmetric ingest loss** | **Refused**, behind `allow_lossy`. §9.6 says of ingest loss "no number here is safe"; a design that concludes that must not then emit a delta. This is the one place marking is insufficient. |
| Mismatched sketch layout | **Refused.** Arithmetically wrong, and provable from a recorded fact. |
| `__overflow__` group rows | **Suppressed**, never compared — different populations by construction (§3.3). |

### 5.7 Percentile convention

**Lower quantile: rank = ⌊1 + q(n−1)⌋, 1-indexed** — for `sampled`, for the sketch, and for
the repaired `traces.slow`.

This is the convention DDSketch's accuracy guarantee is *stated against*
(Masson–Rim–Lee, PVLDB 12(12), Def. 1), and the crate implements it. Rev 2 specified
`ceil(p/100 × n)` — the *upper* quantile — which differs by one order statistic: p95 disagrees
for ~90 % of n, p99 for ~98 %. That made V13's "matched rank convention" unsatisfiable and
voided the ±α bound the result quotes. There is also no public bucket iterator, so
"the sketch's quantile function over cumulative bucket counts" was not implementable; use
`DDSketch::quantile()`.

---

## 6. Snapshots and history

### 6.1–6.2 Operations

`collectors.snapshot(name, label?, description?, meta?, reset=true)` records, returns, and by
default zeroes. `collectors.reset` discards without recording. `collectors.history(name,
limit?, merge?)`, `collectors.get(name, snapshot=label)`, and `collectors.diff` taking
`collector` or `collector@label`.

**A failed persist must not lose the run.** `reset=true` returns the snapshot bytes in the RPC
response regardless, and **does not zero unless the write succeeded** (zero after a successful
`rename`). ENOSPC is realistic when 50 snapshots share a volume with `state.json` and the log.

Labels `[A-Za-z0-9._-]`, unique per collector; auto-labels `snapshot-<n>` never reuse numbers
after eviction. `max_snapshots` 50, FIFO, `snapshots_evicted` reported.

### 6.3 What a snapshot records

Policy `{per_name, per_group, projections, raw_sample_bytes}`, plus — always, regardless of
policy — the exact tier, the sketches **with layout identity**, the `ingest` block, and the
collector's **`filter`, `level`, `group_keys`, `max_sample_bytes`, `description`, `meta`, and
policy as of that moment** — everything a later reader or diff would otherwise take from the
live collector. Without
those, `collectors.diff` would read the *live* collector's current definition and call it
proof.

### 6.4 Disciplines

Validated at `collectors.add`; size quoted up front from the derived sketch and sample
figures; each snapshot explains its own gaps; completeness and ingest loss travel with the
data.

### 6.5 Merging and the two floors

Exact scalars and sketches are additive, so `merge` combines runs — **exact only across
identical sketch layouts**; mismatches refused. Sample-derived projections do not merge and are
omitted with a reason.

Two distinct floors, and a result must not conflate them:

- **Run-to-run floor** — min, max, and CV across repeats of one configuration. Covers
  scheduling variance only; it does **not** cover ingest loss, truncation, or thermal drift.
- **Measurement-resolution floor** — §5.6's ~2 % on estimated percentile deltas.

**One suppression threshold, and it is the one printed.** Rev 2 struck through deltas using a
different bound than it displayed.

Single-run arms report the floor as **unknown**, never absent.

### 6.6 Descriptions and `meta`

Per collector and per snapshot; `meta` carries provenance logmon cannot infer (commit, build
profile, configuration) and is per-snapshot because arms may differ in it.

---

## 7. Contract surface

`collectors.add | list | get | snapshot | history | edit | reset | remove | diff | document`,
plus `traces.profile`. MCP mirrors 1:1; CLI mirrors MCP 1:1.

**Names** use `is_valid_name` (`[A-Za-z0-9_-]`, non-empty) — the rule session and domain names
already share. §10 puts collector names in filenames, in the directory holding `state.json`,
`daemon.pid`, and `config.json`, so `/` or `..` would be path traversal, and `@` would break
`collector@label`.

**Filter admission** replaces rev 2's `is_span_filter` reuse, which returns false for bare
patterns, attribute-only filters, and bookmark windows — all legal span filters. The predicate
is "contains no log-only qualifier", plus:

- **Blocked** — and the bar is *provably never matches*, nothing looser:
  - `ParsedFilter::None`.
  - `d>=NaN`, `d<=NaN`, `d>=+inf`, `d<=-inf`. (`"nan".parse::<f64>()` succeeds, and every
    comparison against NaN is false; `duration_ms` is always finite.)
  - **An empty duration interval** — `max(Gte thresholds) > min(Lte thresholds)`, strict, so
    `d>=100, d<=100` still matches exactly 100.0. Qualifiers are AND-ed, so `d>=100, d<=50`
    can never match, and it is a plausible transposition of "between 50 and 100 ms".
  - **Not `d<=+inf` or `d>=-inf`.** Rev 3 blocked these under the same heading; they match
    **everything**, which is the opposite of the stated bar. They are no-ops and are *marked*,
    not blocked. Stating a bar and then violating it in the same sentence is how the last
    over-correction began.
- **Marked, per qualifier — not per filter.** Every `AdditionalField` selector is named in the
  response, always. Rev 3 marked only when *no* span-specific qualifier appeared anywhere, so
  `sn=cache.lookup, SV=store_server` went unmarked and the AND still zeroed the collector for
  forty minutes. `parse_selector`'s catch-all turns every typo into `AdditionalField`, and the
  parser's existing typo rule only fires on an lhs ending in `>`/`<`.
  The mark also states that **non-string attribute values never match on the span path** —
  `matcher.rs` reads attributes through `.and_then(|v| v.as_str())`, so `cache.enabled=true`
  against an OTLP `BoolValue` matches nothing *for any pattern*. §3.3 fixes this for
  `group_keys`; the filter path is not fixed here, and the mark is what stops it being silent.
  (§4.2 already budgets a `matches_pattern` change in phase 1; aligning the span attribute arm
  with the log arm's `other.to_string()` fallback is the real fix and belongs there.)
- **Echoed**: `BarePattern` scans **`span.name` only** on the span path, versus every field on
  the log path. And **`st=` substring-matches error messages** — the status arm returns
  `pat_lower == "error" || matches_pattern(pattern, msg)`, so `st=ok` also admits an errored
  span whose message contains "ok" (`"broken pipe"` does). A collector armed `st=ok` to measure
  only successful spans will quietly include failures and report `error_count > 0` in a
  population the user believes is clean.

**Bookmarks are rejected in collector filters** (a pre-parsed filter would freeze a stale seq
bound). `traces.profile` accepts `b>=`/`b<=` and **rejects `c>=`** — a cursor is
read-and-advance, so a "profile" that mutates a cursor would make a second identical call
return less, contradicting V22. Rev 2 justified the filter loosening partly with
bookmark-windowed arms, a case §7 makes impossible; that justification is withdrawn.

### 7.1 `collectors.edit` — one rule

Rev 1 refused every structural edit. Rev 2 permitted them. Rev 3 wrote an eight-row table that
a false-negative lens then found wrong in **both** directions simultaneously — partial zeroing
that left `wall_ms` describing the sample tier while `exact.*` covered the whole run, three
paths around the daemon reservation, and a `level`-lower refusal with no principled basis
given that the raise is made safe by zeroing. Three over-corrections on one surface is the
signal that per-field enumeration is the wrong shape.

**The rule:**

> **`description` and `meta` are free. Every other edit is a `reset` plus a config change:
> it swaps the entire collector state — exact tier, sketches, sample tier, interner,
> cardinality counters, ingest baseline, `zeroed_at` — and re-runs every gate that
> `collectors.add` runs.** History is untouched; snapshots are immutable and carry their own
> definition (§6.3).

Everything the table was trying to express falls out, in the permissive direction:

- **`filter`, `group_keys`, `level` up or down** — all permitted at any time. There is no
  mixture to fear because the window is zeroed, and no history to invalidate because §6.3
  records each snapshot's own `filter`, `level`, `group_keys`, and policy, and §5.6 *marks*
  definition differences rather than refusing them. This restores the `level`-lower case the
  lens argued for: a `tree` collector heading for the cap mid-suite can drop to `timing` for
  2.5× the records, which is the only remedy available once the daemon-wide reservation is
  exhausted at four collectors.
- **Reservation is re-checked** on every structural edit, closing the three paths that walked
  past it: a free `max_sample_bytes` raise, a `scalar`→`tree` raise that created a sample tier
  with no reservation at all, and a `snapshot` policy edit changing `raw_sample_bytes`, which
  §3.4 counts against the budget.
- **Every add-time gate re-runs**, not just §7's. Rev 3 said "the full §7 admission gate", but
  §7 is name and filter admission only — the level/metric compatibility checks live in §11 and
  the reservation in §3.4, so a `snapshot` policy naming `self_ms` on a `timing` collector was
  unvalidated.
- **`level_raised_at` is deleted**, along with §5.6's row consuming it. That row excluded the
  raised side's `self_ms` "since its early records predate the parent columns" — a rev-2
  leftover assuming zero-fill. Under zeroing there *are* no early records, and because the flag
  was sticky and survived `reset`, two snapshots taken entirely after a raise had `self_ms`
  excluded from every diff forever. The rule that made the raise safe made its output
  undiffable.
- **`zeroed_at` moves with the swap**, so `wall_ms` (§5.1) and the ingest baseline describe the
  same window as the data beside them. Rev 3's partial zeroing inflated `achieved_concurrency`
  by dividing a whole-run numerator by a post-edit denominator, and produced
  `sample_count < count` with `complete: true` — a second unmarked exception to the one
  reconciliation rule the design has.

**Immutable, and the errors say so:** the sketch layout parameters (`alpha`, range,
`max_name_sketches`, `max_group_sketches`) — §5.6 refuses diffs across mismatched layouts, so
editing them would poison every existing snapshot's comparability at a stroke — and `name`,
which §10 uses as the persistence filename.

**`domain` is re-pinnable, but only while zeroed.** §10 states that a collector pinned to an
API-created domain is *always* orphaned after a restart, since those domains are never
re-created. Without a re-pin the sanctioned response to the design's own default failure mode
would be deleting the collector and its history. A restored collector is zeroed by definition,
so the edit rule already permits it.

**Ordering, because three rules collide here.** §7.1 wants the swap inside the collector lock;
§3.6 forbids I/O under the lock; §10 makes `edit` write-through; and §6.1 establishes that a
destructive operation must not commit until the write succeeds. Satisfy all four with a
per-collector edit mutex held across the whole operation: validate under the lock, persist
outside it, then swap under the lock only after a successful `rename`. Otherwise an edit whose
persist fails has zeroed the live collector while the on-disk definition still holds the old
filter — rev 2's "write-through did not compose" failure in a new place.

*(One rev-3 hazard was retired by the §3.6 probe rather than by this rule: `matched == 0` was
aliased by swap-and-fold, since any `collectors.get` emptied the live generation and a
collector that had matched millions read as virgin. Non-destructive reads now clone, so the
live counters are never zeroed by a read.)*

---

## 8. Threshold triggers (phase 5) — built, not reused

`threshold: { metric, group?, op, value, window_ms }`, evaluated against a **rolling bucket
ring advanced by span arrival** — not a timer, so an idle collector costs nothing and the
zero-CPU-at-idle contract holds.

Nothing is reused: rev 2's cited debounce is the session storage window, the real per-trigger
counter measures *entries*, and span triggers are not debounced at all.

Stated rather than discovered: **with no traffic a breached threshold neither fires nor
clears.** A threshold is a load-time guard, not a liveness check.

---

## 9. Documenting a collector

`collectors.document(names, format, path?, question?, finding?)`.

### 9.1–9.2 Two moments; render, not commit

**Now**, it is the synthesis that says what moved and what to do next; **months later**, triage
and trust. `path` is optional — omit it and the document is returned, which matters because
**`finding` normally arrives after the first read**. Regeneration is free and lossless. No
import: comparison happens in the reader.

`collectors.get` answers a question you have; the document tells you which to ask. The name is
`document`, not `insights` — logmon computes comparisons and rankings; the insight is the
reader's.

### 9.3 The reader's five questions

Is this about my thing? · Can I trust it and is it comparable? · What did it conclude? · Real
or noise? · **What should I do next?** — the last is what the present-tense moment adds, and
why §9.6 is written as instructions.

### 9.4 Front-matter

Fixed-schema YAML, `grep`-able. **No field is ever merely absent** — `null` / `unknown` /
`varies` are explicit values. `git_sha` and `build_profile` are **per-arm**. `question`,
`finding`, `filter_intent` carry the triage weight; `build_profile` is promoted out of `meta`
because comparing debug against release is worse than no data.

**`correctness_evidence`** is an always-present field defaulting to `unknown`. A faster arm may
be faster because it is wrong, and logmon cannot know — but "we asked and nobody supplied it"
is a different statement from silence.

**`aggregation`** is always present: which run each headline figure comes from, or `mean` /
`median` across runs. Rev 2's document showed single figures against a multi-run claim, and a
reader could not reconstruct the floor — making the floor simultaneously load-bearing and
unverifiable.

`trustworthy: false` whenever any arm is single-run, truncated, has ingest loss, or reports
`nesting: "unknown"` — an arm that looked and found none (`"undetected"`) is clean.

### 9.5 Body order

Most-decision-relevant first, reference last, and **every caveat travels with the number it
qualifies**.

1. **What moved** — ranked, `total_ms` and `self_ms` side by side, each row carrying its own
   error bound, sub-threshold deltas suppressed in place. Ranked tables state they are top-N,
   carry an `other` row, and give **both** "share of improvement" and "change to this row"
   (4.4 % and −8.4 % are the same line, and readers conflate them).
   **`avg_ms` deltas are suppressed when `count` differs materially between arms** — an
   exact-tier number that is invalid as an effect size, because the denominator moved.
2. **What to do next** — §9.6 and the floors.
3. **Per-snapshot detail**, every table declaring its tier. When `complete: false`, that arm's
   `sampled` percentiles are **visually demoted**, not printed at equal weight beside
   `estimated` ones they may differ from by 40 %.
4. **The full vocabulary** — and, when ingest loss is non-zero, a note that the counts
   reconcile to `count` by construction and so prove consistency, not completeness.
5. **Definitions and how to read the numbers.** Round 2's cold reader confirmed this section
   changes conclusions — without it they would have recommended acting on the number — so it
   stays, with the collector-config block demoted below the tier definitions.

### 9.6 Limitations — per metric, with remedies

Rev 2 listed limitations; a reader took the truncation row ("does not affect `total_ms`,
`count`, `estimated`") at face value and concluded the exact tier was clean, while the ingest
row said drops affect every tier. Both true, jointly misleading. So the document carries a
**per-metric effect matrix** — for each reported number, which limitations touch it — alongside
the remedy list:

| Limitation | Remedy |
|---|---|
| Ingest loss (`drops_in_window`, `shed_batches`, `malformed_dropped`) | Spans lost **before the collector**; `matched` may under-count and every tier is affected. Reduce load, raise the channel, re-run. |
| Arm truncated | Self time and paths are a **mixture**. Raise `max_sample_bytes`, re-run. |
| `nesting: "unknown"` | Below `tree`, `total_ms` may double-count and nothing can tell you. Re-run at `tree`. |
| `nested_matches > 0` | `total_ms` double-counts; quote `self_ms`. |
| Unmatched descendants | Attributed to the nearest matched ancestor — reads as that ancestor's work. |
| `overlapping_child_spans > 0` | Tree not properly nested; `Σ(unclamped) = self_ms − overlapping_child_ms`. |
| Estimated percentiles | ±α each; **±α(a+b)/\|a−b\| on a delta**; not resolvable below ~2 %. |
| Single-run arm | Floor unknown — repeat. |
| Warm-up excluded | `exact` and `estimated` both null. |
| **`total_ms` *and* `self_ms` under N-way parallelism** | Both are work, not elapsed time; see `achieved_concurrency`. |
| Spans outside the filter | Absent entirely. |
| Several services | Clock skew reaches wall union and path timings. |
| Time, not correctness | See `correctness_evidence`. |
| Retried OTLP batches | No `span_id` dedup. |

Arms run back-to-back are confounded with time; the document shows chronological order and
recommends **interleaved A/B/A/B**.

### 9.7 Sizing

Document in the tens of KB; bulk to a sidecar named in front-matter. **Raw sketch buckets are
not in the sidecar** — `Store.bins` is `pub(crate)` with no public iterator. The sidecar
carries a percentile table plus layout identity, which is what §9.3's questions actually need.
Exposing raw buckets would need an upstream PR; not budgeted.

### 9.8 Formats

`md` (default), `json`, `folded`. Folded emits **integer** microseconds with a single ASCII
space — speedscope's importer matches `^(.*) (\d+)$` and **drops non-matching lines silently**,
while flamegraph.pl accepts decimals, so decimals would render in one tool and vanish in the
other. `;` in span names is escaped to `,`. The sidecar carries the invocation
(`flamegraph.pl --countname us --nametype Span`), since collapsed-stack format has no unit
concept. `[?]` renders as a literal root frame; the how-to-read section says so.

### 9.9 Who writes

**The daemon returns bytes; the client writes** — matching `logs.export`, whose writes are in
the MCP server and CLI. The broker runs as a service, so a relative path would resolve against
the daemon's cwd.

---

## 10. Lifecycle, persistence, restart

**One file per collector, holding definition *and* history**, written write-through on
`collectors.add`, `edit`, and `snapshot`. Rev 2 made snapshots write-through and left
definitions to graceful shutdown, so a `kill -9` produced orphan history with nothing to
attach it to and V11 failed by construction.

**Atomicity:** serialize outside the collector lock and off the runtime; write a temp file
**in the same directory**, `fsync` it, `rename`, then `fsync` the directory. A startup sweep
removes crash-orphaned temps — the existing sweep removes only `daemon.pid` and the socket.

**`state.json` gets the same treatment.** It is `to_string_pretty` + non-atomic `fs::write`
today, and `load_state` propagates a parse error with `?`, so **a truncated `state.json` means
the daemon refuses to start until a human deletes it.** Temp-then-rename, and quarantine-and-warn
on load rather than abort.

**Restore.** Collectors come back **armed but zeroed**, `zeroed_by: "daemon_restart"`; history
is unaffected. The pinned domain is recorded in the collector file, since `PersistedSession`
has no domain field and `restore_named` hard-codes `default`.

**The orphan check is lazy, evaluated on first read** — not at restore. `restore_named` runs at
`server.rs:228` and the domain registry is not created until `server.rs:379`, so a check at
restore time would mark *every* collector orphaned, including those pinned to `default`.

**Restart survival applies to `default` and config-declared domains only.** `domains.create
(persist=true)` is refused today and ephemeral domains are never re-created, so a collector
pinned to an API-created domain is **always** orphaned after a restart. Rev 2 presented
orphaning as an exception; for the primary workflow it is the default outcome, and the spec
says so plainly rather than implying durability it cannot deliver.

**Lifetime and GC.** Session TTL disposal removes the collector and **deletes its file** — rev
2 said both that disposal takes history and that snapshots persist independently of the
session, which cannot both hold and would leak files forever on a daemon whose sessions are
disposed on a 24-hour sweep. A file with no definition, or a definition with no file, is
quarantined and logged at boot.

Duplicate collector name within a session → error. `collectors.add` on an **anonymous** session
→ error (§4.4).

---

## 11. Error handling

Loud and specific. Beyond §7 and §7.1: metric above level; unknown group key; arming exceeds
the daemon reservation (stating requested, remaining, and the four-collector ceiling); snapshot
policy above level; unknown snapshot label; `diff` refusals (layout, asymmetric ingest loss,
both-truncated) and markings (level, filter, group_keys); cardinality and sketch caps;
sample budget hit; ingest loss present; `nested_matches == 0` suppressing `self_ms`; malformed
timestamps; negative durations; out-of-range sketch input; snapshot **persist failure** (does
not zero); `document` folded below `tree`; zero matches returning a well-formed empty result.

---

## 12. Test list

**Verification.** V1 per-match memory re-derived from `size_of`, sketch memory measured ·
V2 exact scalars across the cap · V3 `sampled` percentiles against a hand-computed ⌊1+q(n−1)⌋
reference · **V4 self time by clipped interval union** on concurrent children, a child
outliving its parent, and a child starting before its parent · **V4b** distinguishes one large
overflow from many small ones via `overlapping_child_spans` · V5 wall union · V6 paths and
`[?]` · V7 `group_by` including **boolean and numeric** attributes and `__absent__` ·
V8 warm-up origin and both tiers null · V9 diff bounds using α(a+b)/|a−b| · V10 reset atomicity
and `wall_ms` from `zeroed_at` · **V11 restart including `kill -9`, with definition *and*
history intact** · V12 `traces.slow` over the full population, display floor after aggregation,
rank per §5.7 · V13 sketch vs sample-exact at a matched convention and stated minimum n ·
V14–V17 snapshot/history/merge/descriptions · V18 document completeness incl. per-arm
provenance, `aggregation`, `correctness_evidence` · V19 both floors, one printed threshold ·
V20 sizing · V21 bytes-vs-path · V22 lossless regeneration · V23 body order and per-table tier
· **V24 admission, split in two.** *Blocked:* `ParsedFilter::None`, `d>=NaN`, `d<=NaN`,
`d>=+inf`, `d<=-inf`, `d>=100, d<=50`. *Marked, never rejected:* `SV=`, `message=`,
`cache.enabled=true` on a boolean attribute, `d<=+inf`. Bare patterns, attribute-only filters
and `ALL` admitted. Implementing the mark list as rejection re-creates rev 1's over-block
through the test suite · **V25 zero-duration spans counted and
reported as 0.0; negative durations excluded from sketch and `total_ns` and counted.**

**Adversarial.** A1 cap boundary · A2 parent cycle · A3 arrival order, complete **and under
both cardinality caps** · A3b arrival order truncated: exact and sketch identical, sample rows
may differ · **A4 concurrent reset, edit, and snapshot during ingest** · **A5 domain rebinding
*and* domain deletion mid-collection, asserting data flow rather than the label** · A6/A6b
group and name caps · **A6c a span longer than the declared sketch range — no low-end
collapse** · A7 diff level/filter/layout/overflow handling · A8 incomplete side · A9 idle CPU ·
A10 truncation not laundered into a snapshot · **A11 per-field sum invariant across *every*
structure — `count`, `total_ns`, `sample_count`, sketch count, per-name, per-group,
`error_count`** · A12 history eviction · **A13 a structural edit — filter, `group_keys`, level up *and* down — on a
collector with prior matches: every structure zeroed together, `zeroed_at` and the ingest
baseline moving with it, history untouched, and A11 still holding afterwards. Rev 3's partial
zeroing made A13 and A11 contradict each other** · **A14 observer effect: criterion
bench over `process_span_for_domain` at 0/1/4 collectors; a saturation test asserting zero
receiver drops; a read-during-ingest test at the cap; stated reps and threshold.**

**Infrastructure phase 1 needs and the repo lacks:** criterion (no `benches/`, no
`[[bench]]`, no `#[bench]` anywhere). **Phase 3 needs** a crash harness — the test daemon runs
in-process as a tokio task, so there is no process to `SIGKILL`, and `restart()` is a graceful
shutdown that would pass V11 regardless. Real-ingest tests must use `spawn_in_dir_no_inject`;
the default harness leaves the span channel idle.

---

## 13. Build order

1. **Core.** The §3.6 lock discipline (chunked, 8 192-record chunks), exact tier with `i128`, DDSketch with enforced range and
   zero/negative rules, **the new `ReceiverMetrics` counters**, domain-keyed registry,
   `add`/`list`/`get`/`reset`/`remove`, `traces.profile`, admission (§7), the §4.2 matcher
   de-allocation, criterion, and the §1.1 `traces.slow` fix.
   **Phase 1 ships the complete `ProfileResult` field set**, later-phase fields present and
   inert — adding them later is a wire break.
2. **Projections.** Clipped-union self time, paths, wall union, warm-up, `group_keys`.
3. **History.** Snapshot/history/edit, per-collector files with atomic writes, lazy orphan
   check, GC, crash harness, merging.
4. **Comparison.** `collectors.diff`; `collectors.document`.
5. **Guards.** Threshold triggers.

Phases 1–3 are the complete driving workflow. Docs are part of done; note the MCP surface grows
33 → 44 tools, roughly doubling per-session schema context.

Deferred: log-derived durations; cross-collector baselines; multi-key `group_by`; `span_id`
dedup; raw sketch buckets in the sidecar (needs an upstream PR).

---

## 14–15. Gate rounds 1 and 2

Round 1 lenses: buildability · soundness+false-positive · performance+prior-art · cold reader.
Round 2: concurrency+durability · **false-negative** · prior-art+numbers · cold reader.

Two lens choices earned their place and should be reused on any similar design:

- **The cold reader**, given only a synthetic document and barred from the spec, produced
  findings no spec-reading lens could — and in round 2 confirmed the how-to-read section
  changes a reader's recommendation, which is two-way evidence for keeping it.
- **The false-negative lens in round 2**, aimed specifically at checks loosened in response to
  round 1. Every one of the four had been over-corrected. Loosening a check in response to a
  false-positive finding should always be followed by this lens.

## 16. Round 3 — reduced scope

New in rev 3: §3.6's lock discipline — **probed, not merely reviewed** (§3.6); §5.3 clipped
interval union; §5.1.1's new receiver counters; §7.1's exhaustive edit rules; §10's
single-file write-through and lazy orphan check.

**§3.6 was probed instead of reviewed — and the probe changed the design.** That surface was
on its third iteration (coarse lock → chunked-with-split-locks → swap-and-fold), which made it
the highest-prior place for a defect, and each earlier version had *read* correctly to a
reviewer. A running experiment settled it in one pass: swap-and-fold over-reports self time,
chunk-cloning is exact, and both the lock-hold cost and the chunk size now rest on measurements
rather than on a plausible sentence. **Where a design question is executable, execute it** —
a fourth reader on the same surface would have been the weaker instrument.

That correction is also the third instance of one pattern: rev 2 over-corrected four checks in
response to false-positive findings, and rev 3 over-corrected the *whole concurrency mechanism*
when only its lock scope was wrong. **When a reviewer finds a defect, replace the defect, not
the mechanism.** Rev 2's chunked tier was right in shape; it needed one lock, not a rewrite.

**Still open:** a false-negative pass on §7.1, which has now been tightened and loosened once
each. A cold-reader pass is **not** needed — rev 3's document changes are additive to a shape
round 2 validated.

---

## 17. Implementation status — updated 2026-07-29

Branch `feat/span-time-collector`. **Phase 1 is complete.** Everything below is
committed, with the full workspace suite green (63 result lines with
`--features logmon-broker-core/test-support`), clippy silent, and
`cargo xtask verify-schema` clean. The protocol crate now carries the `Profile*` and
`Collectors*` types and the schema has been regenerated.

The end-to-end test is the one worth re-running first after any change here: it posts
real OTLP over a real socket and checks hand-computed totals, so it fails on a broken
seam rather than a broken unit.

### Landed

| Piece | Where | Notes |
|---|---|---|
| Criterion bench harness | `crates/core/benches/span_ingest.rs` | The §12 A14 dependency the repo lacked. |
| Trigger-path fix | `TriggerManager::evaluate_span` | Pulled into phase 1 from a filed chip once the bench proved it a prerequisite (§4.2). |
| Duration sketch | `collector/sketch.rs` | §3.5 — DDSketch, ns, enforced range, layout identity. |
| Exact tier | `collector/exact.rs` | §3.1 — `i128`, malformed/negative accounting. |
| Sample tier | `collector/sample.rs` | §3.2/§3.4 — chunked, 8 192 records, prefix truncation. |
| Interning | `collector/intern.rs` | §3.3/§3.5 — caps with `__overflow__` / `__absent__`. |
| Collector state | `collector/state.rs` | §3.6 — one lock, clone-read, swap. |
| Registry + ingest wiring | `collector/registry.rs`, `daemon/span_processor.rs` | §4.4 — domain-keyed; `Arc` built in `run_with_overrides`, held by `RpcHandler`. |
| Filter admission | `filter/admission.rs` | §7 — blocks and marks, both directions mutation-tested. |
| Span-loss counters | `receiver/metrics.rs` + both OTLP paths | §5.1.1 — `record_trace_batch_shed`, `record_trace_malformed`, `trace_ingest_loss()`. |
| Projections | `collector/project.rs` | §5 — all four categories, percentiles, clipped-union self time, wall union, paths, warm-up. |
| Wire types | `protocol/methods.rs` | §5.1 — the **complete** field set; core builds them directly, so there is no mirror to drift. |
| Contract surface | `daemon/rpc_handler.rs` | §7 — `collectors.add\|list\|get\|reset\|remove`, `traces.profile`. |
| MCP surface | `mcp/server.rs` | 6 tools, 1:1 with the RPC methods. 33 → 39. |
| CLI + SDK mirror | `mcp/cli/collectors.rs`, `sdk/methods.rs` | §7 — `logmon-mcp collectors add\|list\|get\|reset\|remove\|profile`. |
| End-to-end | `tests/collector_end_to_end.rs` | Real OTLP/HTTP → real daemon → collector → RPC. Hand-computed numbers, exact match. |
| `traces.slow` repair | `daemon/rpc_handler.rs`, `span/store.rs` | §1.1 — full population, display floor, §5.7 rank, folded inside the read guard. |
| A14 | `benches/span_ingest.rs`, `tests/collector_concurrency.rs` | Observer effect measured *and* asserted. |

### Deployed — v0.4.0, tag `v0.4.0`, commit `074e366`

Phase 1 is **live** in `~/.cargo/bin/{logmon-broker,logmon-mcp}`, with the launchd
service (`logmon.broker`, `KeepAlive`) restarted onto it. `cargo install --path … --force`
was used rather than copying, so cargo's `.crates.toml` bookkeeping stays truthful.

**`PROTOCOL_VERSION` stays 1.** `server.rs` compares it for **exact equality** in the
handshake, so bumping it would refuse every client not upgraded in the same instant.
Everything in phase 1 is additive on the wire. Verified in the field: a 0.3.0 shim
running since two days before the deploy reconnected to the 0.4.0 broker and resumed
its session with its triggers intact.

**A running MCP shim does not gain the new tools.** The tool list is compiled into
`logmon-mcp` and served once at MCP `initialize`; replacing the file on disk does not
touch a process already running. A parallel session must restart its shim. The CLI is
unaffected — it spawns a fresh process per invocation, so `logmon-mcp collectors …`
works immediately.

**Building later phases cannot disturb this.** The deployed binaries are copies, so
`cargo build` in the tree never reaches them. Verified rather than assumed: `state.json`
in the live config dir is byte-identical, with an unchanged mtime, across a full
workspace test run.

**The one hazard ahead is phase 3.** It writes per-collector files into the directory
holding `state.json`, `daemon.pid` and `config.json` — the live daemon's directory.
Every test touching persistence must go through `DaemonOverrides.config_dir` (there is
no env var or flag for it); a helper that calls `config_dir()` directly would write
into the running daemon's state. The daemon-level path is already guarded — a second
broker in that directory refuses to start on the pid lock — so the exposure is a unit
test that writes files without starting a daemon at all.

### Corrections this section makes to §5

Each found by making the spec run. The spec text above is left as written; these
are the deviations, and they are deliberate.

1. **Self time cannot come out negative** (§5.3). Every clipped child is a subset of
   its parent's interval, so their union measures at most the parent's duration — the
   clamp is unreachable and `overlapping_child_ms` would be provably always zero,
   which reads as "no anomalies" when it means "cannot fire". The pair now reports the
   **clipped-away mass** and the number of children it came from: the anomaly clipping
   *hides* (a child that started before its parent or outlived it — clock skew or an
   instrumentation bug). Still answers V4b. The clamp survives as a documented
   unreachable guard.
2. **`achieved_concurrency` is sampled-over-sampled** (§5.1). The spec's
   `total_ms / wall_union_ms` divides the exact tier's total by the sample tier's
   union; under truncation those are different populations and the ratio is inflated
   by exactly the truncation ratio. They agree when `complete`, which is the only case
   §5.1 had in mind.
3. **`nesting` has a third state**, `unknown`. Below tree level the question cannot be
   asked, and `undetected` would let a reader infer a flat call structure from a
   retention setting.
4. **Children are keyed on `(trace_id, span_id)`.** Span ids are only unique *within* a
   trace; sequential-per-trace instrumentation collides constantly across traces.

### Phase 3 — landed

| Piece | Where | Notes |
|---|---|---|
| Snapshot history | `collector/history.rs` | §6 — labels never reused, eviction counted, merge + run-to-run floor. |
| Persistence | `collector/persist.rs` | §10 — one file, definition *and* history; sketches round-trip. |
| Atomic writes | `daemon/persistence.rs` | §10 — temp → fsync → rename → fsync dir, boot sweep, quarantine-not-abort. |
| Restore | `CollectorRegistry::restore` | §10 — armed but zeroed, `zeroed_by: "daemon_restart"`. |
| Lazy orphan check | `handle_collectors_list` | §10 — at read time, never at restore. |
| `collectors.edit` | `CollectorRegistry::edit` | §7.1 — description free, everything else a reset. |
| GC | `remove` / `drop_session` | §10 — disposal deletes the file. |

Three things worth knowing before touching this again:

- **A description edit must carry the sample tier across.** `Collector::with_def`
  rebuilds `Inner` around a new definition, and the first version left
  `Inner::new`'s fresh `SampleTier` in place — which gives `sample_count < count`
  with `complete: true`, the one reconciliation rule this design has, broken by a
  rename. Caught by re-reading, not by a test; there is a test now.
- **The `state.json` atomicity fix was load-bearing for more than collectors.** A
  truncated file used to stop the daemon booting until a human deleted it.
- **A rename-vs-copy guard needs a concurrent test.** `copy` then `remove` leaves
  byte-identical final contents; only a reader racing the writer can tell them
  apart. The single-threaded version of that test passed against the mutant.

### Deep gate — run 2026-07-30 on `4115c46`, nine defects, all fixed in `123f9b1`

**Merged and deployed: `master` at `123f9b1`, tag `v0.5.1`, live daemon 0.5.1.**
Phases 1–3 complete. Three fresh-context finders over the frozen diff:
line-by-line (sonnet), cross-file tracing (strongest), test-validity by mutation
(sonnet, own worktree).

The gate found nine defects in code that was already **tested green, clippy-clean,
and deployed**. That is the evidence for keeping it, stated as a number.

| Defect | Class |
|---|---|
| A large `group_keys` list aborts the process (eager `8192 × width × 4` alloc, budget-blind; `traces.profile` never passes the reservation gate at all) | **remote DoS, one call** |
| Four budget-leak paths: `kill -9`, `session.rename`, `session.drop` ordering, rename displacement | lifecycle |
| Two `fsync`s under the lock that gates every domain's ingest | latency → span loss |
| `edit --domain` left the metrics handle behind → false "domain is gone" | wrong output |
| Boot temp sweep non-recursive, missing the files this feature added | leak |
| `skip_warmup_ms` overflow → cut silently a no-op | wrong output |
| Mutual parent cycle corrupts self time | wrong output |
| `snapshot` could error away a run it had already swapped out | data loss |
| Corrupt file underflows `well_formed_count` | wrong output |

**Facts worth not re-deriving:**

- `MAX_GROUP_KEYS = 8`. The group-key column is the one dimension
  `max_sample_bytes` does not cover, and `Vec::with_capacity` answers an
  allocation failure with `handle_alloc_error` — an **abort**, not an unwind.
- **Never write a collector file under the registry lock.** `ingest_span` takes
  it for reading on every domain. Serialize under it, write outside it.
- **The metrics handle must move with the pin.** Attribution is `Arc::ptr_eq`.
- **A collector's owner must be a session the daemon can reach.** Collector files
  are write-through; session names reach `state.json` only on graceful shutdown.
  `restore` re-registers the owner to reconcile the two regimes.
- The mutation finder proved three green mutations **inert** rather than reporting
  them as gaps — one via a 200 k-trial differential fuzzer. Distinguishing
  untested from untestable is most of that lens's value.
- `.claude/` is now git-ignored: an agent worktree was committed as an embedded
  repo and caught in the commit output.

### Phases 4 and 5 — landed, 0.6.0

**`collectors.diff` (§5.6).** Three arm shapes from one syntax: `c` (live), `c@label`
(one run), `c@*` (every run merged). `*` cannot be a valid label, so the wildcard can
never collide. The merged shape is not a convenience — it is the only one that yields a
run-to-run floor, so with single-run arms on both sides every threshold reads `unknown`
and §6.5's "one printed threshold" has nothing to print.

Four decisions worth not re-deriving:

- `Arm` holds breakdowns keyed by **resolved label**, not interner id. A merged arm spans
  snapshots that each carry their own interner, so ids would graft one run's `db.query`
  onto another's — a plausible number with no error.
- A merge refuses to combine three things: sample-derived projections (a self time across
  two runs is not a self time), per-axis breakdowns **unless every run recorded them** (a
  partial breakdown cannot reconcile to the total, breaking A11 undetectably), and ingest
  loss when any window could not attribute it (unknown, not the sum of the known ones).
- The **nesting verdict is carried explicitly**, not derived from `projections`. Deriving
  it made every merged arm read `unknown`, so every multi-run diff advised "re-run at
  `tree`" to someone already at `tree`. Caught by a test, not by inspection.
- Run-to-run floors are **per metric**. A fixed-iteration suite has near-zero count
  variance while its timings vary by percent, so a duration CV applied to a count row
  strikes out a real five-span difference in three hundred.

**`collectors.document` (§9).** The cold-reader lens ran and was worth every minute: it
found eight things, two serious, and it found a **regression introduced 20 minutes
earlier while acting on an earlier finding** — "did not change at all" is provable for an
exact count and forbidden for an estimated percentile, because two equal sketch outputs
mean only that both fell in the same bucket. Fixing one side of a message left the other
unsampled, which is exactly the failure the ways-of-working skill warns about.

The other findings are all now guards: every active limitation gets a column in the reach
table (it had two columns for six limitations, so a `-` read as "clean" when the truth was
"no column"); absent numbers read `n/a` rather than being certified; a merged arm states
that its figures are **sums**; the ranked table has a count column, because without it a
reader cannot tell "fewer of them" from "each one cheaper"; and the threshold column names
its basis rather than silently switching between the two floors §6.5 says must not be
conflated.

The reader also named what NOT to cut, which is half the value: the DDSketch ±1% sentence
(it stopped a false bug report about an estimated p80 exceeding the exact max — 0.76%
over, inside the bound), "all 3 rows" (what made the by-name sum usable as proof), the
per-run **range** and not just the CV (the only way they discovered the totals were sums),
the two `git_sha`s, and §4's insistence on printing zero counters.

**Paths are stored as frames, not a joined string.** A span named `parse > eval` is
indistinguishable from two frames once ` > ` is the separator, and the flame-graph
renderer that splits the joined form apart would emit a stack that never ran. `folded`
therefore works on a recorded run (the user chose this over erroring, since no snapshots
existed on disk yet).

**Threshold triggers (§8).** The ring lives outside the collector's `RwLock` — all
atomics, so ingest never needs the write lock that gates every domain's ingest. Pull-only
reporting, by the user's choice: the verdict rides on `list` and `get`, and on `get` only
for a **live** read, because a recorded run has no rolling window and reporting the live
verdict beside a closed window's numbers would attach a fact about now to a window that
ended.

A threshold change is **structural** and zeroes the window (§7.1), because the ring is
measurement state. Two defects the tests caught: `registry.edit` never applied
`change.threshold` to the new definition (reported `zeroed: true`, kept the old limit),
and the handler built the report as a `json!` literal, emitting `"last_value": null` where
the schema promises the key is absent — and absent-versus-null is the whole distinction
that field carries.

**One spec/implementation discrepancy, resolved toward the substance — and since
corrected in the spec.** §3.2, §9.4 and §9.6 all wrote `nesting: "undetected"` where they
described the *below-`tree`* case, which the code calls `"unknown"` and whose remedy
("re-run at `tree`") only makes sense there. `"undetected"` is reserved for a `tree`-level
run that looked and found none. Implemented per the bodies; the three labels were fixed
2026-07-30.

`FORMAT_VERSION` and `PROTOCOL_VERSION` both stay at **1**. Every addition is optional and
defaulted, and the only version gate refuses a file *newer* than the running build — so a
defaulted field reads correctly in both directions, where a bump would make every existing
snapshot unreadable in exchange for nothing.

### Not yet built

1. **§4.2 matcher de-allocation** — confirmed real but **0.2 % of per-session ingest
   cost**; deliberately deprioritised now that measurement has ranked it.
2. **~~A real `kill -9` harness.~~ BUILT 2026-07-30** —
   `crates/broker/tests/kill_dash_nine.rs`, out-of-process against the real binary.
   `LOGMON_CONFIG_DIR` was the missing piece: `config_dir()` now resolves
   `DaemonOverrides` > env > `$HOME` default, and the SDK's `default_socket_path()`
   reads the same var, so one variable redirects a daemon *and* its clients. The
   harness spawns with GELF port 0 (kernel-assigned) and both OTLP ports 0
   (disabled), so it can never collide with a developer's live daemon.

   **The thing that cost time, worth not re-deriving: a socket FILE existing is not
   readiness.** A Unix socket is not unlinked on process exit, so a daemon that bound
   and then died leaves a file that answers every later `connect` with
   `ECONNREFUSED` — which is precisely the state this test exists to distinguish, and
   waiting on the file reported it as ready. Readiness is a `connect` that succeeds,
   plus a `try_wait` on the child so a dead daemon is reported as dead with its log
   rather than as a timeout. Never send a spawned daemon's output to `/dev/null`.

   Mutation-verified: removing `.with_persistence(dir)` from `server.rs` turns the
   test red with `{"collectors":[],"count":0}`.
3. **~~`status.get` does not surface the new counters.~~ BUILT 2026-07-30** —
   `trace_ingest { dropped, shed_batches, malformed_dropped }`, a **sibling** of
   `receiver_drops` rather than a member: that field documents itself as counting
   *silent* loss, and a shed batch was a 429 the caller saw. Scoped to the bound
   domain, exactly like `receiver_drops`.

   **`dropped` is a projection, not new state** — literally
   `receiver_drops.otlp_http_traces + otlp_grpc_traces`, the same two atomics
   (`metrics.rs:264`). Repeated so the three trace figures read as one block, and
   documented on the wire type because **summing it with those fields
   double-counts.** Only `shed_batches` and `malformed_dropped` are numbers nothing
   else in the payload reports. The implementing agent found and corrected this
   against a brief that had assumed independence; the test asserts the real
   relationship in two stages rather than the tidier false version.
4. **A rolling percentile threshold.** Refused rather than deferred: it needs a duration
   sketch per bucket, and the per-collector memory bound is the reason this design can
   promise anything about its own cost. The error says so and names `avg_ms`.
5. **The document's per-arm `wall_ms` on a merged arm is a sum**, and is labelled as such
   rather than replaced with an elapsed time. A real elapsed figure across merged runs
   would need the gaps between them, which nothing records.
6. **Micro-retro is owed** for this branch, and the log is past five entries since the
   last `## Consolidated through` marker — so `/process-retro` is due before the next
   feature starts, not after.

### Facts that cost time to establish — do not re-derive

- Ingest baseline, measured: 3.94 µs at 0 sessions, 34.6 µs at 1, 118.6 µs at 4 —
  **now 3.91 / 4.35 / 4.66 µs** after the trigger fix.
- **Observer effect, measured** (1 bound session): **4.10 / 4.39 / 4.93 µs** at 0 / 1 / 4
  collectors — about **0.21 µs per collector**, 20 % at the four-collector ceiling.
- Chunk size 8 192 is measured, not chosen: 0.29 ms read lock-hold vs 1.80 ms at 65 536.
- DDSketch's rank is `(q * (n-1)) as u64` — matches §5.7, no adjustment needed.
- `Config::min_possible()` **is** public (a gate report said otherwise).
- `Config: PartialEq` and `merge` already refuses mismatched configs — but the layout
  **record** is the load-bearing guard, since `Config` does not survive persistence.
- Four test files carry **pre-existing** rustfmt drift (`domains_binding`,
  `log_processor`, `session_registry`, `trigger_window_defaults`). Not mine; leave them.
- Taking the **read** lock in `CollectorRegistry::reset` does not compile — the baseline
  update needs a mutable borrow. The borrow checker enforces there what a test samples.
- On the **OTLP** transports `port = 0` means *disabled*; it means kernel-assigned only
  for GELF. A real-receiver test has to bind free ports itself.
- `receivers_info` entries are `UDP:` / `TCP:` / `gRPC:` / `HTTP:` — not `OTLP-HTTP:`.
- The state directory is overridable **only** through `DaemonOverrides` (the test
  harness). There is no env var or flag, so a second daemon cannot be started alongside
  a developer's own — use `spawn_with_real_receivers_config`.

### Working discipline that has been paying

Every guard is mutation-tested before commit. That has found a dead guard, three vacuous
or non-discriminating tests, one missing test (the reset re-baseline), and **five**
distinct ways a mutation harness can lie: patch miss, semantically inert patch,
misclassified cargo output, non-terminating mutation reported as a compile failure, and
a filter that matched no tests reported as `ok`. Scripts live in the session scratchpad;
the pattern is worth recreating rather than the files.

Two harness lessons worth keeping: mutate **one half** of a compound guard at a time (it
proves each half separately *and* still terminates), and `cargo test -p <crate> <filter>`
filters by test **name** — `--test <file>` selects a target.
