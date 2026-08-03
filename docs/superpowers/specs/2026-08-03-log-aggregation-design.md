# Log aggregation — architecture, and `logs.fields`

**Supersedes `2026-08-02-log-aggregation-design.md`**, which a three-lens design gate
found unsound: three of its four built-in axes are dead in real data, its extraction
stage contradicted unanimous prior art, and it designed past **B4**, an existing user
proposal for the same problem with a different goal. That document is not committed;
this replaces it. §6 records where each finding landed.

**Tier:** T2 — mints RPC contracts.
**Status:** architecture settled with the user 2026-08-03. `logs.fields` designed here;
the other three pieces get their own designs.

---

## 1. Architecture — three questions, three tools

The superseded spec collapsed three questions into one method. Separating them is what
the gate and the user's own review converged on.

| Tool | Question | Returns |
|---|---|---|
| **`logs.fields`** | *What dimensions exist in here?* | One row per field: coverage, distinct, top values, type |
| **`logs.profile`** | *How do records distribute along **this** dimension?* | Rows keyed by axis value: count, levels, exemplar, optional values |
| **`logs.recent(collapse)`** | *Show me the records, minus the repeats* | The same records, folded by signature — **this is B4** |

### Why B4 lives on `logs.recent`

`profile` describes a population and returns **statistics**. B4 returns **records** — the
read you were already doing, with duplicates folded. The original proposal
(`2026-06-30-…-proposal.md`, B4) put it there and said it *"composes with B3 (ack a
whole signature)"*: you ack records, not statistics. Folding it into `profile` would
give one method two unrelated reply shapes — the defect that disqualified shape 2.

**Measured on the live buffer 2026-08-03**, which is what settles B4 as live and narrow:

| Filter | Records | Signatures | Collapse |
|---|---|---|---|
| `l>=WARN` | 454 | 374 | 1.2:1 |
| `l>=ERROR` | 23 | 4 | **5.8:1** |

B4 pays off on errors — 23 records are 4 distinct problems, exactly its stated scar —
and barely on warnings. It is a triage tool aimed at `l>=ERROR`, not general machinery.

### Build order (user-agreed 2026-08-03)

1. **`LogStore::for_each_matching`** — the seam all of this needs (§2)
2. **`logs.fields`** — small, no extraction, and it makes everything after it usable
3. **`logs.profile`** — named axes and counts, no values yet
4. **The extraction stage** — separately designed; also unblocks span-attribute
   aggregation and armable log collectors
5. **B4 on `logs.recent`** — independent of all of the above

### Why `logs.fields` goes first

An agent must name an axis to use `profile`, and an agent arriving cold does not know
which axes exist. The superseded spec required step two and skipped step one — then its
author hand-ran step one to write it, which is how the dead-axis defect got in.

**`logs.fields` is the tool that would have caught that defect.** It reports actual
presence, so a dead axis shows as 0% instead of returning a silent empty bucket.

---

## 2. The shared seam (carved out)

`LogStore` has **no full-walk primitive**. Every read returns a cloned `Vec<LogEntry>`
(`store/traits.rs:12–37`), and `recent_with_scanned` **early-stops at `count`**
(`store/memory.rs:99–101, 113–115`). So the natural implementation of any
whole-population read is the wrong one and looks right on any fixture smaller than the
default.

The span side already solved this: `SpanStore::for_each_matching`
(`span/store.rs:137–152`) streams without cloning.

**Add `LogStore::for_each_matching`**, mirroring it. Both `logs.fields` and
`logs.profile` need it; neither can be correct without it. It ships as its own step so
the two features share one reviewed primitive rather than each growing a walk.

---

## 3. `logs.fields` — design

### Method

`logs.fields` — MCP `list_log_fields`, CLI `logmon-mcp logs fields`.

### Parameters

| Param | Type | Default | Notes |
|---|---|---|---|
| `filter` | string | `ALL` | Existing DSL, bookmark-resolved. Describes *the population you are about to profile*, so it must accept the same filter |
| `top_values` | integer | 3 | Top values reported per field |
| `min_coverage_pct` | number | 0 | Omit fields present on fewer than this share. `0` reports everything, including 0% |

**Cursor qualifiers (`c>=`) are rejected**, mirroring `traces.profile`
(`rpc_handler.rs:3037–3040`). A cursor advances on read, so a second identical call
would describe a different population — and `logs.recent` accepts and commits cursors,
so inheriting its parsing without the rejection would silently do that.

### Reply

Top level: `scanned` (the whole ring, §2), `matched`, `buffer_total`, and the evidence
fields the sibling reads already carry — `truncated`, `evicted_before_window`, and a
`verdict` defaulting to `cannot_verify` (`methods.rs:240–268, 343`). A description of a
population that will not say whether the population was complete is the defect this
whole family exists to avoid.

Per field row:

- `field` — the name an axis would be spelled with
- `present` / `coverage_pct` — records carrying it, over `matched`
- `distinct` — distinct values, or `null` with a `suppressed` entry past the cardinality
  cap (reusing `collector::intern`, `intern.rs:23`)
- `top_values` — `[{value, count}]`
- `kind` — `string` / `integer` / `float` / `bool` / `mixed`, from the observed values

`kind` is not decoration: it is what tells a caller which fields the extraction stage
(step 4) can sum. On the live buffer that distinguishes `cold_start_batch_count`
(integer) from `target` (string) without the caller guessing.

### Built-ins are reported beside additional fields, with their real presence

`level`, `host`, `facility`, `file`, `line` are reported as ordinary rows. This is the
point of the tool, not a courtesy: GELF strips `_` and routes prefixed keys into
`additional_fields` (`gelf/message.rs:210–214`), while `LogEntry.file`/`line`/`facility`
come from top-level keys many emitters never send. On the live buffer `fi`, `ln` and
`fa` match **0 of 6161** records while `file` and `line` match nearly all — as
*additional fields*. A field summary makes that visible in one call.

`trace_id` and `span_id` get rows too, marked as promoted: they are `.remove()`d from
`additional_fields` at parse time (`gelf/message.rs:216–224`), so an agent grouping by
them would otherwise get a silent 100%-absent bucket.

### Ordering

By `coverage_pct` descending, then by field name — stable across reads, following
`project.rs:1254–1266` ("equal rows are stable across reads"). Leaving ties to
iteration order is the reproducibility bug this project already ruled against once.

---

## 4. Test plan

| # | Property | Seam | Tool / level | Catches |
|---|---|---|---|---|
| F1 | Coverage and distinct are exact over a known fixture | the projection fn over a `Vec<LogEntry>` | unit | Arithmetic |
| F2 | **`scanned` covers the whole buffer, not `count` records** | `for_each_matching` (§2) | unit, fixture larger than any plausible default | The §2 trap — the natural implementation early-stops and looks right on small fixtures |
| F3 | A built-in absent from the data reports 0%, not omission | fixture with no top-level `file` | unit | The dead-axis class: silence reading as "no such field" rather than "field exists, never populated" |
| F4 | `trace_id`/`span_id` appear despite being removed from `additional_fields` | fixture with trace ids | unit | The promoted-field blind spot |
| F5 | Past the cardinality cap, `distinct` is `null` + `suppressed`, never a wrong number | fixture above the cap | unit | A capped count reading as exact |
| F6 | `kind` is `mixed` when values disagree | int and string under one name | unit | A wrong type steering the extraction stage at a field it cannot sum |
| F7 | A cursor qualifier is rejected | `c>=x` | unit + RPC | Silent cursor advance between two identical calls |
| F8 | Rows are stable across two identical calls | equal-coverage fields | unit | Iteration-order dependence |
| F9 | The reply carries `verdict`/`truncated` | live daemon | `crates/core/tests/` | Shipping a population description with no completeness channel |

### Negative controls

- **F2** — cap the walk at 50 records; F2 goes red and F1 stays green. If F1 also goes
  red its fixture is smaller than the cap and proves nothing.
- **F3** — omit absent fields instead of reporting 0%; F3 red. This reproduces the
  original defect (a dead axis being invisible), not merely a changed output.
- **F5** — report the partial count instead of `null`; F5 red while F1 stays green.

---

## 5. The other three pieces

Each gets its own design; sketched only so the shape is on record.

- **`logs.profile`** — named axis, counts, levels, exemplar, `groups_total` before
  truncation. Rows must **reconcile with `matched`** (a reserved absent bucket, spelled
  `__absent__` per `intern.rs:18`, not a new `missing`) — the cold reader caught the
  superseded spec's own example failing this by 24 records.
- **The extraction stage** — a *named* derived value, defined once, then aggregated as
  an ordinary field. Prior art is unanimous (`rex → stats`, `regexp → unwrap`,
  `parse → sum`, `GROK → STATS`). Separating it is what lets a captured value be
  grouped by and filtered on, removes the per-call cap, and gives log collectors an
  armable definition. It should be designed knowing **spans will consume it too**:
  span attributes are group keys only today (`collector/state.rs:393` stringifies
  numbers), so spans cannot sum their own attributes either.
- **B4 on `logs.recent`** — `collapse: "signature"`, returning `{count, first_seq,
  last_seq, sample}`. The signature definition is the whole design: on live data,
  including `file:line` split one `run script failed` into two rows at 9× each, which
  is either correct or over-separation depending on intent.

---

## 6. Gate findings — where each landed

**Resolved by the architecture:** B4 designed-past (now §1, its own feature); extraction
fused into the aggregate (now step 4, separate); no-axis shape absent from the option
set (now `logs.fields`, step 2); V7/§10/§7 internal contradictions (the sections are
gone).

**Carried into this spec:** dead built-in axes (§3, and F3 tests it); promoted
`trace_id`/`span_id` (§3, F4); missing evidence verdict (§3 reply, F9); cursor
qualifier not rejected (§3, F7); `scanned` unimplementable on the existing primitive
(§2, F2); tie-break unspecified (§3 ordering, F8); `missing` vs `__absent__` (§5).

**Deferred to the pieces that own them:** `i64` overflow, `admit_log_filter` +
`warnings`, exemplar selection, `first_seen`/`last_seen` ordering, the `group_by`
open-set-versus-schema-enum problem — all belong to `logs.profile` or the extraction
stage, and are recorded in §5 rather than solved here.

**Method changes adopted:** duty 0 now includes *"has this been specified before?"* — a
grep over `docs/superpowers/specs/` before designing, which would have found B4 in
thirty seconds. And a probe must answer its claim's own question: the superseded spec's
L7 counted `additional_fields` keys and was used to support claims about *selector*
axes.

---

## 7. Definition of done

Per the standing rule, all three narrative artifacts in the same session:

| Artifact | Needs |
|---|---|
| `skill/logmon.md` | Reach for `logs.fields` **first** when you do not know what is in the buffer; the call shape; that a 0% row means the field exists but is never populated |
| `README.md` | A tool-table row, and the three-questions framing of §1 |
| `docs/medium-article.md` | Why an agent needs a map before it can measure |

Code-side gates the superseded spec omitted: request/reply structs in
`protocol/src/methods.rs`; a `Tool` entry in `mcp_tools.rs` (that list **is** the
surface, pinned by `capability_skew.rs`); `cargo xtask gen-schema` + `verify-schema`;
and a `render::for_method` arm (`render/mod.rs:185–216`).
