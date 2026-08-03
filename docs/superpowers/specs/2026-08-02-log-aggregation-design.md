> # ⚠ SUPERSEDED — do not implement from this document
>
> Replaced by **`2026-08-03-log-aggregation-design.md`** after a three-lens design
> gate on 2026-08-02 found it unsound. Kept only because parts of it are worth
> salvaging; read it as source material, never as a specification.
>
> **What was wrong:** three of §6's four built-in axes are dead in real data
> (`fi`/`ln`/`fa` match 0 of 6161 records — GELF routes prefixed keys into
> `additional_fields`); §6.3 fuses extraction into aggregation, which unanimous
> prior art separates; §5's option set was padded, as the document itself admits;
> and the whole feature was designed past **B4**, an existing user proposal for the
> same problem with a different goal. §6.4's own worked example fails its own
> completeness invariant by 24 records.
>
> **What is worth keeping**, and what the extraction-stage design should start
> from: §6.3's `contributed`/`unparsed` per-value denominators, the null-not-zero
> rule, and §8's fixture-asymmetry reasoning in the negative controls. All three
> gate reviewers independently called these the strongest part of the document.

# Log aggregation — design (superseded)

**Tier:** T2 — mints a new RPC method, an externally-binding contract.
**Status:** design, pre-implementation. Architect pass complete; shape not yet chosen
by the user.

---

## 1. Problem

The log surface is retrieval-only: `logs.recent`, `logs.context`, `logs.export`,
`logs.clear`. There is no way to ask *how many* or *of what kinds* — only to fetch
records and look at them.

### The failure mode this produces in the primary consumer

`get_recent_logs(count=N, filter=…)` returns the N **most recent** matches. When a run
produced thousands of matching records across a handful of distinct shapes, an agent
sees a recency-biased sample, forms an impression, and reports it as though it were a
count — "the errors are mostly X". Nothing in the reply bounds what was missed.

The span side already refuses to do this. A profile reports `groups_total=13`, so a
reader can tell "the top 13 of 13" from "the top 13 of 900", and every figure that
could not be computed arrives as `null` beside a `suppressed` entry explaining why.
The log side offers no equivalent, so the bias is invisible rather than bounded.

This is the project's own *count, don't eyeball* rule, unenforceable on logs because
the daemon exposes no way to count.

---

## 2. Load-bearing claims

| # | Claim | Status | Evidence |
|---|---|---|---|
| L1 | The log RPC surface has no aggregation | confirmed | enumerated served methods: `logs.clear`, `logs.context`, `logs.export`, `logs.recent` only |
| L2 | `LogEntry` carries level, message, host, facility, file, line, trace/span ids, and open `additional_fields` | confirmed | `crates/core/src/gelf/message.rs:91–120` |
| L3 | The filter DSL already names the groupable axes, including arbitrary additional fields | confirmed | `Selector` enum, `crates/core/src/filter/parser.rs:166–180` (`AdditionalField(String)`) |
| L4 | A proven cardinality cap with `__overflow__` folding exists and is reusable | confirmed | `crates/core/src/collector/intern.rs:12,23` (`OVERFLOW_LABEL`, `DEFAULT_GROUP_VALUE_CAP = 1024`) |
| L5 | The codebase has an explicit precedent **against** normalising where the normaliser could be wrong | confirmed | `crates/core/src/collector/diff.rs:1723–1727`, pinned by `diff/tests.rs:850` |
| L6 | The project's logging policy makes `kind` the failure-family key and says to query by it rather than regexing prose | confirmed | `~/.claude/skills/logging-policy/SKILL.md:47–51` |
| L7 | In live data, exact axes are near-universal and `kind` is partial | confirmed by probe | 400 sampled records: `service`/`app`/`target`/`file`/`line` on 400; `kind` on 102; levels `{Warn: 254, Info: 146}` |
| L8 | No message normalisation / templating exists anywhere in the tree | confirmed | grep for normalis*/template/drain/cluster over `crates/core/src` returns only unrelated hits |
| L9 | Logs carry no duration, so there is no `total_ms` analogue to rank by | confirmed | L2 — the record has a timestamp, not an interval |
| L10 | **Level is not a `Selector`** — it parses to `Qualifier::LevelFilter`, a separate form | confirmed | `parser.rs:16–18`, `:307–325`; absent from the `Selector` enum at `:166–180` |
| L11 | **`sn`/`sv`/`st`/`sk` never match a log** — the log matcher returns `false` for all four | confirmed | `crates/core/src/filter/matcher.rs:110–112` |
| L12 | There is no `admit_log_filter`; only `admit_span_filter` exists | confirmed | `crates/core/src/filter/admission.rs:143` — so no existing function defines the log-valid axis set; this spec must |
| L13 | **Live logs already carry numeric structured fields, in both integer and float form** | confirmed by probe | 60 sampled records: `cold_start_batch_count`, `cold_start_instruction_count`, `span_count`, `peak`, `live`, `owners` as `int`; `cold_start_apply_ms`, `cold_start_wait_ms` as `float` |
| L14 | `groups_total` already exists with **two** distinct meanings | confirmed | `protocol/src/methods.rs:1665` and `:1830`; the schema notes `CollectorsDiffResult`'s counts comparable keys across two arms with `__overflow__` excluded, and is "not expected to reconcile" with the profile's |

---

## 3. Prior art

Consulted before generating shapes, as the design rules require. These are from
general knowledge, not verified in-session, so no single one carries weight — **the
convergence is the signal**:

| Tool | Exact faceting | Approximate clustering |
|---|---|---|
| Splunk | `stats count by <field>` | `cluster` — a separate command |
| Elasticsearch | `terms` aggregation | `categorize_text` — a separate, ML-backed feature |
| Loki / Grafana | `sum by (label)` | "log patterns" — a separate, later feature |
| Datadog | facets / log analytics | "Log Patterns" — a separate view |

Every one of them ships exact faceted aggregation as the workhorse and keeps
approximate pattern clustering as a **distinct, explicitly-approximate** feature. None
merges the two into one operation. That is the shape to copy.

---

## 4. The honesty constraint — and a correction to an earlier recommendation

**In the conversation that led to this spec I recommended grouping by message template
as the core of the feature, calling it "the only part that isn't already reachable via
export + shell". Three findings overturn that ranking.**

1. **L6 — the project already has an exact failure-family key.** The logging policy
   puts `kind` on every ERROR and WARN and says, in as many words, to query by `kind`
   rather than regexing prose. Message-template clustering would heuristically
   re-derive what `kind` states exactly.
2. **L5 — the codebase has ruled on this class.** Two regexes that compile to the same
   matcher canonicalize *differently* and mark, deliberately, because "a mark that says
   'these differ in spelling' is honest while a normaliser that got it wrong would not
   be." A template normaliser that over-merges reports two distinct failures as one row
   with `count: 2`, and the reader concludes one bug where there were two. Same class.
3. **L7 — exact axes are already present on effectively every record.** `target`
   (the Rust `tracing` module path) is on 100% of sampled records and is a strong
   proxy for "what kind of log is this", at zero heuristic cost.

Where `kind` is missing, template clustering is at its least reliable *and* the honest
remedy is to add `kind` — which the policy already requires — not to guess at shapes.

### The exemplar is the honest substitute for a template

Rather than synthesising `connection to <IP> failed` (which may be wrong), each group
carries **one real message, verbatim**, beside its count. The reader learns what the
group is without a second query, and nothing is claimed that was not observed. This is
L5's "mark rather than normalise" applied to logs.

Template clustering is not rejected forever — it is deferred to §9 with the conditions
that would justify it.

---

## 5. Shapes

| Shape | Trades | Risk class **deleted** | Testability seam | Failure surface | Cost to reverse |
|---|---|---|---|---|---|
| **1. `logs.profile` — a new retrospective read** | One new method; mirrors `traces.profile`, which readers already know | Sampling bias: `groups_total` + caps make the unseen population *stated* rather than invisible | Pure projection over a record slice — unit-testable without a daemon | Empty result vs "no groups" must not conflate; cardinality blowup on a high-arity field | Low — additive method |
| **2. Extend `logs.recent` with a `summarize` flag** | No new contract | Same as 1 | Same | One method returning two unrelated reply shapes; every client branches on a flag | Medium — the flag is in the contract |
| **3. A forward-facing log collector** (arm / snapshot / diff) | Symmetric with span collectors; enables across-run diffing | Nothing 1 doesn't; *adds* arming, persistence, budget | Needs the whole collector lifecycle | Large new surface: state, TTL, disk | High |
| **4. Auto-pick the grouping axis** | No `group_by` to choose | None — adds a heuristic | Hard: the choice is the behaviour | Picks the wrong axis and looks authoritative | Medium |

**Recommendation: shape 1.**

It matches the prior-art shape (§3), reuses machinery that already exists (L4), and is
the only option that is purely additive. Shape 2 saves a method name and pays with a
permanently branched reply shape. Shape 3 is a real feature but it answers "did this
change between runs", not "what is this run full of" — and it should follow 1, not
replace it, since a collector wants a projection to collect. Shape 4 adds exactly the
kind of guessing §4 argues against.

**Shape 3 is the structurally different option** and is worth stating plainly rather
than padding the list: if the goal were regression-tracking of log volume across
builds, 3 would be the right answer and 1 the wrong one.

---

## 6. The design (shape 1)

### Method

`logs.profile` — MCP `profile_logs`, CLI `logmon-mcp logs profile`.

### Parameters

| Param | Type | Default | Notes |
|---|---|---|---|
| `filter` | string | `ALL` | Parsed by the existing filter parser; bookmark-resolved, exactly as `traces.profile` does |
| `group_by` | string | none | One axis from the accepted set below — **not** simply "any `Selector`" |
| `top_n` | integer | 20 | Rows returned, ranked per `rank_by` |
| `values` | array | none | Up to 8 named quantities to extract, each `{name, field}` **or** `{name, capture}` — §6.3 |
| `rank_by` | string | `count` | `count`, or the `name` of one requested value (ranks by its `sum`) |

#### The accepted axis set, and why it is not "any `Selector`"

An earlier draft of this section said `group_by` takes "a `Selector` spelling,
including `l` and `sv`". Checking that against the code falsified it twice (L10, L11),
and the corrected set has three tiers:

| Axis | Spelling | Note |
|---|---|---|
| Level | `level` | **Not a `Selector`** (L10) — level is a `Qualifier::LevelFilter`, a separate parse form. Grouping by it reads `LogEntry.level` directly and is a deliberate special case, not a vocabulary member |
| Log built-ins | `h`, `fa`, `fi`, `ln` | Host, facility, file, line — the `Selector` variants the log matcher actually resolves |
| Additional fields | any other name | `kind`, `target`, `service`, `app`, `ty`, … — **the axis that matters most** (L7), so it is the fallback rather than a special case |

**Excluded, each for a reason rather than by omission:**

- `sn`, `sv`, `st`, `sk` — **span-only**. The log matcher returns `false` for all four
  (L11), so grouping logs by them would produce one empty result with no explanation.
  Note the trap this retires: live logs carry `service` and `span_name` as *additional
  fields* (L7), so `service` is a valid axis while `sv` is not — the names look
  interchangeable and are not.
- `m`, `fm`, `mfm` — raw message text. Cardinality is effectively one key per record,
  so every result would fold to `__overflow__`. Grouping by unnormalised message is
  also the naive move §4 argues against: it under-merges as badly as a bad normaliser
  over-merges.

A rejected axis names the accepted set (G7). Given that `sv` is both plausible-looking
and wrong, the rejection message for a span-only selector says so specifically and
points at the additional-field spelling.

### 6.3 Numeric aggregation

Added after the user raised a case §1 did not cover: a message reading
`Items processed: 10`, where the question is *how many items in total since a
bookmark* — not how many records said so. Counting records answers the wrong
question, and the original design could only count records.

**One extraction, four statistics.** The caller names a *value*, not an operation;
the reply carries the whole stat block. This mirrors what `traces.profile` already
prints for durations (`total`, `avg`, `min`, `max` — observed live), so a reader who
knows one profile reads the other without learning new vocabulary.

Two extraction paths per value, deliberately distinct:

| Path | Exactness | When |
|---|---|---|
| `field` | **Exact** — the value is already structured | You control the emitter, or it already emits `items_processed=10`. **This is not hypothetical here** (L13): the live store already carries `cold_start_batch_count`, `cold_start_instruction_count`, `span_count`, `peak`, `live` and `owners` as integers and `cold_start_apply_ms` / `cold_start_wait_ms` as floats — every one of them summable today with no emitter change |
| `capture` | **Approximate** — parsed out of prose | Retrospective analysis of logs already captured, or an emitter you do not own |

`capture` is admitted rather than refused, for the reason the shell workaround already
demonstrates: people will extract these numbers anyway, and doing it in the daemon at
least makes the caveats reportable. But it is marked, because a change to the message
format silently changes the number and nothing else would say so. Where the emitter
*is* yours, the honest fix is a structured field — the same argument the logging
policy makes for `kind`, applied to values.

Each entry gives exactly one of `field` or `capture`. A `capture` requires exactly one
capture group; zero or two is a parse error naming the count found, not a silent
first-group guess.

#### Several values per call

One log line often carries more than one number worth totalling
(`processed 512 items in 340ms`). Extracting them in separate calls would walk the
buffer twice and — worse — leave the two results unjoinable, because nothing would say
the extremes came from the same record.

`values` is a list of named quantities; the reply is keyed by those names. Rules the
plural forces:

- **`name` is required and unique**, and `count` is reserved (it already means the
  record count). Names key the reply, so a positional scheme would make the output
  unreadable and a reordering silently repoint every caller.
- **Each value carries its OWN `contributed`/`unparsed`.** A record may yield `items`
  and not `ms`. One shared denominator would be wrong for at least one of them, and
  wrong in the direction that overstates.
- **Capped at 8.** Each value is a regex or a lookup per record; the cap keeps an
  ad-hoc read from becoming an unbounded scan. Exceeding it is an error naming the cap,
  not a silent truncation.

**The payoff is the join.** When `items.max_seq` and `ms.max_seq` are the same seq, the
biggest batch was also the slowest — a correlation neither value shows alone, and one
that two separate calls could not establish at all.

#### Ranking, and a decision reversed

An earlier draft ranked rows by `value.sum` whenever a value was requested. **With
several values that is ambiguous**, and every way of resolving it implicitly is worse
than asking:

- Ranking by the *first* entry makes array order load-bearing, so reordering a list
  silently changes which rows are returned.
- Making the default conditional on the number of values means adding a second value
  silently reorders the output.

So `rank_by` defaults to **`count`** always, and names a value when you want its sum.
This costs the single-value case one extra parameter and buys a rule that does not
change under you. Ranking a summed quantity by record count would otherwise bury the
largest contributor behind the chattiest one — which is exactly why the parameter
exists rather than the ordering being fixed.

#### The denominator is not optional

A sum without its population is the silent-undercount this whole design exists to
prevent. Every stat block therefore carries:

- `contributed` — records that yielded a number
- `unparsed` — records that matched the filter but yielded none: the field was
  absent, the regex did not match, or the text was not numeric

`avg` is `sum / contributed`, **never** `sum / matched`. A reply where
`contributed` is much smaller than `matched` is the signal that the capture or the
field name is wrong, and it is visible without a second query.

**The filter must narrow to the records that should carry the value**, or `unparsed`
is meaningless. A filter of `b>=mark` alone matches every log line since the mark, so
`unparsed` counts the entire unrelated remainder — hundreds of thousands of records —
and the one number that exists to reveal a bad capture instead reveals nothing.
Surfaced by writing the worked example in §6.4, not by review.

Since the caller cannot be relied on to know this, the daemon says it: when
`unparsed > contributed`, the reply carries a `suppressed`-channel note reading *most
matched records carry no such value — narrow the filter to the lines that should, or
these figures describe a subset you did not choose*. The threshold is a heuristic and
the note is advisory; it never changes a figure.

When `contributed` is 0, `sum`/`avg`/`min`/`max` are **`null` with a `suppressed`
entry**, never `0`. Zero is a legitimate sum and must not be how "nothing was
extracted" renders — the same rule §6 already applies to `excluded_by_warmup` on the
span side.

#### `min` and `max` carry the record that produced them

An extreme is only half an answer: *the largest batch was 4,096* invites *which one,
and when?* immediately. So `min` and `max` each carry the `seq` of the contributing
record, and render beside the value:

```
sum 41,207   avg 106.2   min 1 @seq 41127   max 4,096 @seq 41508
```

A `seq` is the project's existing navigation primitive — it feeds `get_log_context`
directly — so this turns a number into a pointer instead of the start of a hunt. It
costs nothing: the extraction pass already visits every record, and carrying the seq
of the current extreme is one assignment per improvement.

**Ties resolve to the LOWEST seq — the first record to reach the extreme.** Specified
rather than left to iteration order because two identical calls returning different
`max_seq` values would be a reproducibility bug that only appears on tied data, and
"whatever the loop happened to do last" is not a contract. `sum` and `avg` have no
such pointer: they are properties of the population, not of any record.

When `contributed` is 0, both seqs are `null` alongside their `null` values.

#### Numeric type

Values accumulate as `i64` while every contribution is integral, promoting to `f64`
on the first fractional one. **Both cases occur in the live store already** (L13) —
`span_count` is an integer field, `cold_start_apply_ms` a float — so the promotion
path is exercised by real data rather than being defensive design. Summing integers through `f64` throughout would silently
lose precision past 2^53 — irrelevant for item counts, real for byte totals — and a
counter that goes wrong only above a threshold nobody tests is exactly the defect
class this project writes `suppressed` channels to avoid. Promotion is recorded so
the renderer can format without a spurious `.0`.

### 6.4 Worked example — the call an MCP client actually sends

The question: *how many items were processed in total since the run started?*, where
the number lives in prose (`Items processed: 10`).

Two calls. First, before the run, mark the point:

```json
add_bookmark({ "name": "before-run", "description": "start of the import run" })
```

Then, afterwards, ask:

```json
profile_logs({
  "filter": "b>=before-run,m=/Items processed/",
  "values": [ { "name": "items", "capture": "Items processed: (\\d+)" } ]
})
```

A line carrying two numbers — `processed 512 items in 340ms` — is one call, not two,
and grouping composes:

```json
profile_logs({
  "filter": "b>=before-run,m=/processed/",
  "group_by": "kind",
  "values": [
    { "name": "items", "capture": "processed (\\d+) items" },
    { "name": "ms",    "capture": "in (\\d+)ms" },
    { "name": "bytes", "field": "payload_bytes" }
  ],
  "rank_by": "items"
})
```

Three things in that filter are load-bearing, and all three were verified against a
live daemon while writing this section:

- **Qualifiers are comma-separated** (`parser.rs:225`). Neither a space nor `AND`
  parses as a conjunction.
- **`b>=` is read-only; `c>=` is read-and-advance.** Use `b>=` for a figure you may
  recompute, or the cursor moves under you between calls. `b>=name` resolves to a
  **strict** `>` against the bookmark's seq, so the marked record itself is excluded.
- **`m=/regex/` is case-SENSITIVE; a bare substring is not.** `m=/items processed/`
  returns nothing where `m=/Items processed/` returns everything, while
  `m="items processed"` matches either way (`matcher.rs:117`).

The second qualifier is not redundant with the capture: it is what makes `unparsed`
mean anything (§6.3).

Adding `"group_by": "kind"` turns the same call into *sum of items processed per
failure family since the run started* — which is the combination that earns the
feature.

### Reply

Top level:

- `matched` — records matching the filter
- `scanned` — records examined. **The whole ring buffer, not a `count`-limited
  window**: an ad-hoc profile that silently covered only the most recent N would
  reproduce the exact sampling bias §1 exists to remove, while looking authoritative.
  `matched/scanned` is therefore a real ratio over the retained population
- `groups_total` — distinct keys **before** `top_n` truncation (the anti-bias field).
  Follows the **profile** convention of the two that already exist (L14): every key
  this axis holds, `__overflow__` **included**. `missing` counts as a key too — it is
  a real bucket of records, and excluding it would make the rows fail to account for
  `matched`. Deliberately *not* the `CollectorsDiffResult` convention, which excludes
  `__overflow__` and counts across two arms
- `levels` — exact count per level across the whole matched set
- `first_seen` / `last_seen` — timestamp and seq bounds of the matched set
- `cardinality_capped` — whether any value folded into `__overflow__` (L4)
- `suppressed[]` — the existing honesty channel, same shape as the span side
- `values` — present only when `values` was given (§6.3). A map keyed by `name`, each
  `{ sum, avg, min, min_seq, max, max_seq, contributed, unparsed, source: "field"|"capture", exact: bool }`

Per group row:

- `key` — the value, or `__overflow__`
- `count`
- `levels` — per-level breakdown within the group
- `first_seen` / `last_seen`
- `exemplar` — **one real message, verbatim** (§4)
- `values` — the same per-name stat blocks as top level, scoped to this group.
  `group_by` + values is the combination that earns the feature: *items and
  milliseconds, per `kind`, since a bookmark*, in one call

`missing` is a distinct key from `__overflow__`: a record that lacks the grouped field
entirely is not the same as one folded past the cap, and conflating them would make the
denominator lie. Both are reserved keys and both are named in the reply.

### What it does NOT compute

**No duration statistics** (L9) — a log record is an instant, not an interval. The
stat block of §6.3 describes an *extracted* value and never a latency.

**No percentiles over extracted values.** `sum`/`avg`/`min`/`max` are computable in
one streaming pass with constant memory; percentiles are not, without retaining every
value. The span side can offer them because a collector deliberately retains samples
at `timing`/`tree` level; an ad-hoc log read retains nothing. Offering percentiles
here would mean either a hidden retention cost or a silently-sampled figure, and the
project's rule is that a figure which cannot be computed is `null` plus an
explanation.

**No rate.** `count`, `first_seen` and `last_seen` are present and a reader can
divide — but a rate the daemon *published* would carry a denominator it cannot vouch
for, since ring eviction may have truncated the window.

**Ranking** is by `rank_by` descending — `count` by default, or a named value's `sum`.
See §6.3 for why the default is unconditional rather than switching when a value is
present.

---

## 7. Reuse

Per the standing rule to reuse proven in-tree machinery rather than hand-rolling:

| Need | Reuse | Citation |
|---|---|---|
| Cardinality cap + `__overflow__` | `collector::intern` | `intern.rs:12,23` |
| Filter parse + bookmark resolution | the `traces.profile` path | `rpc_handler.rs:3025–3034` |
| Per-axis value **location** — not extraction | The log matcher's arms show where each selector's value lives on a `LogEntry`, but **every arm returns `bool`**: `AdditionalField` fetches the value, stringifies it, and matches a pattern. There is no function to call. Read this as a map of where to look, not as reusable code | `matcher.rs:70–112` |
| Groupable axis vocabulary | `Selector`, **minus level (L10) and the span-only four (L11)** | `parser.rs:166–180` |
| Honesty channel (`suppressed`) | the profile projection's | `collector/project.rs` |
| Server-side rendering | `render/profile.rs` sibling | `crates/core/src/render/` |

The extraction step is genuinely new: `Selector` today is a **match** vocabulary, and
grouping needs to **extract a value**. That function is the one real new primitive and
is where the tests point.

---

## 8. Test plan

| # | Property | Seam | Tool / level | Catches |
|---|---|---|---|---|
| G1 | Counts are exact over a known fixture | the projection fn, over a hand-built `Vec<LogEntry>` | unit, `crates/core` | Arithmetic and filter-application errors |
| G2 | `groups_total` reports distinct keys **before** truncation | fixture with 30 keys, `top_n=5` | unit | The anti-bias field silently equalling `top_n` — the whole point of §1 |
| G3 | A record missing the grouped field lands in `missing`, not `__overflow__` | fixture mixing present/absent/high-arity | unit | The conflation §6 calls out |
| G4 | Past the cap, values fold to `__overflow__` and `cardinality_capped` is set | fixture with > cap distinct values | unit | A capped result reading as complete |
| G5 | `exemplar` is a verbatim message from that group | fixture with distinct messages per group | unit | A synthesized or cross-group exemplar |
| G6 | The method is reachable and shaped as declared | live daemon, real RPC | `crates/core/tests/` (the `collectors_rpc.rs` pattern) | Handler wiring; schema/daemon drift |
| G7 | Rejected `group_by` names the accepted set | bad axis | unit + RPC | An unhelpful rejection on the axis most likely mistyped |
| G8 | `group_by: "sv"` is **rejected**, and the message points at the `service` additional field | the span-only arm (L11) | unit + RPC | The trap in §6: `sv` would otherwise return one empty group forever, since the log matcher answers `false` (L11) rather than erroring. A silent empty result is the worst available outcome and the only one the naive implementation produces |
| V1 | `sum`/`avg`/`min`/`max` are exact over a known fixture, by both paths | the extraction fn, over a hand-built `Vec<LogEntry>` | unit, `crates/core` | Arithmetic; a capture group off by one |
| V2 | **`avg` divides by `contributed`, not `matched`** | fixture where they differ — 10 matched, 4 numeric | unit | The headline defect of §6.3. A fixture where all matched records contribute cannot detect it, so the fixture asymmetry *is* the test |
| V3 | `contributed + unparsed == matched`, always | fixtures mixing absent field / non-numeric text / non-matching regex | unit | A record silently counted in neither bucket — which makes both the sum and its denominator wrong in the same direction |
| V4 | `contributed == 0` yields **null** stats plus `suppressed`, not zeros | filter matching records that never carry the value | unit | `sum: 0` reading as a real total. Zero is a legitimate sum; conflating it with "extracted nothing" is the exact conflation the codebase refuses elsewhere |
| V5 | Integer sums stay exact past 2^53 | two values above 2^53 whose true sum is representable only in `i64` | unit | The `f64`-throughout shortcut. Fails only above a threshold, so nothing but a deliberate fixture finds it |
| V6 | A capture regex with 0 or 2 groups is a parse error naming the count | malformed patterns | unit + RPC | A silent first-group guess |
| V7 | Ranking switches to `value.sum` when a value is requested | fixture where count-order and sum-order disagree | unit | Rank-by-count leaving the largest contributor off the page — invisible unless the fixture's two orderings differ |
| V8 | `min_seq`/`max_seq` name the record that actually holds the extreme | fixture where the extreme is **not** the first or last record | unit | A seq that is really "first seen" or "last seen" wearing the extreme's name — which a fixture whose extreme sits at either end cannot distinguish |
| V9 | Tied extremes resolve to the **lowest** seq, stably across repeated calls | fixture with the same max value at three seqs | unit | Iteration-order dependence: a reproducibility bug that appears only on tied data |
| V10 | Each value keeps its **own** `contributed`/`unparsed` | fixture where records yield `items` but not `ms` | unit | A shared denominator — which would overstate `ms.avg` while `items` looks correct, so a single-value fixture cannot detect it |
| V11 | Duplicate or reserved (`count`) value names are rejected | `values` with two `items`, and one named `count` | unit + RPC | A silently dropped value, or a name collision with the record count |
| V12 | `rank_by` naming an unrequested value is rejected, listing the names given | `rank_by: "bytes"` with no such value | unit + RPC | Falling back to `count` ordering while the caller believes they ranked by a value |
| V13 | More than 8 values is an error naming the cap | 9 entries | unit + RPC | Silent truncation presenting a partial extraction as complete |

### Negative controls

- **G2** — make `groups_total` return `rows.len()`; G2 must go red. This reproduces the
  original defect (an unbounded population presented as bounded), not merely a broken
  diff.
- **G4** — raise the cap above the fixture's arity; G4 must go red *and* G1 stay green,
  proving G4 tests folding rather than counting.
- **G1** — the assertion runs inside a loop over groups: it needs a count guard
  (`assert!(checked >= 3)`), or a fixture that produces no groups passes vacuously.
- **V2** — change the divisor to `matched`; V2 must go red **and V1 stay green**. If
  V1 also goes red, its fixture has `contributed == matched` and it is not testing
  what it claims.
- **V4** — return `0` instead of `null` on an empty contribution; V4 must go red. The
  control has to distinguish "the sum is zero" from "there was no sum", so the fixture
  needs a *sibling* case whose true sum genuinely is 0 and which must stay green.
- **V5** — force `f64` accumulation throughout; V5 must go red while V1 stays green.
  Both arms are required: V5 red alone would also follow from a broken sum.

**The recurring shape in these controls is fixture asymmetry.** V2, V5 and V7 each
compare two quantities that a careless fixture makes equal — `contributed` vs
`matched`, `i64` vs `f64`, count-order vs sum-order. Where they are equal the mutation
is unobservable and the test passes while proving nothing. That is the vacuous-scenario
failure the mutation lens exists to catch, and it is cheaper to design out here than to
find later.

---

## 9. Deliberately out of scope

- **Message-template clustering.** Deferred per §4, not rejected. Revisit when: `kind`
  coverage is high and grouping by it still leaves a large unexplained bucket, **or** a
  concrete investigation is blocked by its absence. If built, it ships as a *separate*
  axis that is explicitly labelled approximate and always carries exemplars — never as
  the default.
- **Log collectors** (shape 3) — arm/snapshot/diff over logs. A real feature; it wants
  this projection to exist first.
- **Rate/derivative figures** — see §6.

---

## 10. Definition of done

Code and tests are not the deliverable on their own. All three narrative artifacts are
updated **in the same session**, ticked mechanically against the diff rather than from
memory (user-directed 2026-08-02):

| Artifact | What it needs for this feature |
|---|---|
| `skill/logmon.md` | The call shape an agent emits: `profile_logs` with `group_by` / `value_field` / `value_capture`, the accepted axis set (§6), and the §6.4 gotchas — comma conjunction, regex case-sensitivity, `b>=` vs `c>=`. Also the §1 framing: reach for this instead of paging `get_recent_logs` and eyeballing |
| `README.md` | A row in the tool table, and the "narrow the filter or `unparsed` is meaningless" caveat (§6.3) where a user will meet it |
| `docs/medium-article.md` | Why it exists at all — the sampling-bias story of §1, which is the part a stranger can follow without knowing the tool |

They are not interchangeable: the same change wants a *different* sentence in each,
because the audiences differ.

**Until the daemon-served-skill change lands, editing `skill/logmon.md` is not a
docs-only edit** — it is `include_str!`'d into the shim, so it needs
`cargo install --path crates/mcp --locked` and an MCP client restart before any agent
sees the new text.
