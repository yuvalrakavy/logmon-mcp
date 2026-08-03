# `logs.profile` — how records distribute along a named axis

**Step 3 of the build order** in `2026-08-03-log-aggregation-design.md` §1. That spec
settled the architecture — three questions, three tools — and left each piece its own
design. This is the piece that answers *how do records distribute along **this**
dimension?*

**Tier:** T2 — mints an RPC contract (method, request/reply structs, MCP tool entry,
schema definitions, render arm).
**Shape chosen by the user 2026-08-03:** mirror `traces.profile` — a closed enum names
the *kind* of axis, an open `group_keys` list names emitter fields.
**Deliberately out of scope:** extracted numeric values (`sum`/`avg`/`min`/`max`). Those
are step 4, and §8 states how they attach without a contract change.

**Revision 2, after a four-lens design gate** (grounding audit, implementer+soundness,
architect-pass audit, false-positive lens). §10 records what the gate found and where
each finding landed. Three requirement gaps went back to the user; their answers are in
§2, §5.2 and §5.6.

---

## 1. Duty 0 — the claims this design rests on

Every row was checked before designing; the grounding audit re-checked all 30 citations
in the document and confirmed 27 unchanged. **Two of the parent spec's claims are false**,
and both changed the design.

| # | Claim | Verdict | Evidence |
|---|---|---|---|
| C1 | `__absent__` is the reserved absent bucket | confirmed | `collector/intern.rs:18` |
| C2 | `__absent__` is the only reserved label | **false** — `__overflow__` is its sibling and the parent spec's §5 sketch never mentions it. These two are the *only* reserved labels in the repo | `intern.rs:11-12`; grep of every `*_LABEL: &str` |
| C3 | The `group_by` open-set-vs-enum problem is unsolved | **false** — `traces.profile` already solves it: closed enum for the axis kind, open `group_keys` for names | `protocol/src/methods.rs:1915-1923` |
| C4 | `admit_log_filter` does not exist | confirmed | `filter/admission.rs:143` has only `admit_span_filter`, called from three span-RPC sites |
| C5 | `groups_total` counts keys **before** truncation, `__overflow__` included | confirmed | `methods.rs:1965-1979` |
| C6 | The store walk is oldest-first | confirmed | `store/memory.rs:141` — `inner.entries.iter()`, unreversed |
| C7 | `logs.export` / `logs.recent` return **newest-first** | confirmed by probe | 9347 live records, seq monotonically decreasing |
| C8 | The 1024 value cap is the operative hazard | **false on real data** — the highest real axis is `span_trace_id` at 204 distinct. Only `message` exceeds the cap (4235 of 9347), and message is not a general axis | probe over the live buffer |
| C9 | `__absent__` is an edge case | **false — it is the median case.** `kind` is 86.4% absent; 20 of 31 candidate axes exceed 90% | probe |
| C10 | A per-group level breakdown carries signal | confirmed, partially — 13 of 31 candidate axes have at least one group spanning more than one level | probe |
| C11 | `groups_total` is load-bearing | confirmed **for high-cardinality axes only** — top-20 covers 100% of most axes, but 28.4% of the *additional* field `span_trace_id`, 50.8% of `store`, 80.6% of the **additional** field `line` | probe |
| C12 | `Level` is a small closed set | confirmed — five variants, `Trace=0 … Error=4`. Three same-named severity enums exist in the repo and all three agree on five | `gelf/message.rs:41-47` |
| C13 | A named axis absent from every record renders as an ordinary row, with no error | **confirmed by reproducing it live on `traces.profile`** — a guessed attribute name returned one clean `__absent__` row covering 100% of 4569 spans | live call, 2026-08-03 |

**C11 names its `source` because the gate caught it not doing so.** The 80.6% figure is
the *additional* field `line`; the **built-in** `line` matches 0 of 9347 records, so
profiling it yields exactly one `__absent__` group and top-20 trivially covers 100%. A
claim in this document that did not say which `line` it meant was exhibiting the precise
ambiguity §5.4 exists to fix.

C8 and C9 together invert the parent spec's emphasis: the cardinality cap is a leak guard
that will essentially never fire on real data, while the absent bucket is what almost
every call will be dominated by. C13 is the finding that produced §5.4.

---

## 2. Method and parameters

`logs.profile` — MCP `profile_logs`, CLI `logmon-mcp logs profile`.

| Param | Type | Default | Notes |
|---|---|---|---|
| `filter` | string | `ALL` | The DSL, bookmark-resolved. **Cursor qualifiers rejected before resolution** — §5.5 |
| `group_by` | enum | `none` | `level`, `message`, `host`, `facility`, `file`, `line`, `trace_id`, `span_id`, `field`, `none`, `""` |
| `group_keys` | array&lt;string&gt; | — | Emitter field names, at most `MAX_GROUP_KEYS` (8). **Required non-empty when `group_by` is `field`; an error, non-empty, on any other axis.** More than one forms a tuple |
| `top_n` | integer | 20 | **Real** rows returned; reserved buckets are additional — §5.6 |

### An empty `group_keys` is not a present `group_keys`

The check gates on **non-empty**, never on presence. `Some(vec![])` and `None` both mean
"no field keys requested" here.

This is not a nicety: `group_keys_of` → `opt_str_array` (`rpc_handler.rs:3405-3423`,
`:3669-3687`) deliberately keeps the two apart, and `param_typing_rpc.rs:410-454` asserts
live that on `collectors.edit` an explicit `[]` is *"a legal, structural change"* distinct
from omission. A generated client that always serialises `group_keys: []` would otherwise
have its most ordinary call — `{group_by: "none", group_keys: []}` — refused.

The reason `[]` is structural on `collectors.edit` (it clears persisted state) does not
transfer: this method persists nothing. When it normalises to "no keys", the reply echoes
`group_keys: null`, matching the reply's own convention that `null` means nothing was
asked for.

### Non-empty `group_keys` on a non-`field` axis is a hard error — **user-decided**

The gate found this diverges from the mirror in both directions: the span side pushes a
`Suppressed` entry for empty `group_keys` rather than erroring (`project.rs:826-834`) and
performs no validation at all when `group_keys` accompanies a non-group axis
(`rpc_handler.rs:3152`). It offered warn-and-answer as the alternative, on agent-ergonomics
grounds.

**The user chose the hard error (2026-08-03), against the sibling's behaviour and with
the divergence stated.** Recorded here so it is not re-raised: the mistake is unambiguous,
and a caller who passes field names believes they are grouping by fields — answering a
different question silently is worse than one refused round trip.

### Tuple width is capped at 8, reusing `group_keys_of`

`MAX_GROUP_KEYS = 8` (`collector/sample.rs:78`), enforced at `rpc_handler.rs:3414`. The
existing helper is reused for the parse, the empty/absent distinction and the width check
— but **its error message is span-specific** (*"each one costs a column in every retained
sample… not covered by max_sample_bytes"*), which is meaningless here. The log path
supplies its own text: the cost is one accumulator key per distinct tuple, and eight
members already exceeds any axis this buffer has.

### Why the enum has these arms

They are exactly the rows `logs.fields` reports, so its output is the input to this method
with no translation. `BUILTINS` (`fields.rs:167-176`) holds exactly eight, and the gate
checked the mapping both ways — no `logs.fields` row is unreachable from this enum:

| `logs.fields` row | `logs.profile` axis |
|---|---|
| `source: builtin`, `field: "host"` | `group_by: "host"` |
| `source: promoted`, `field: "trace_id"` | `group_by: "trace_id"` |
| `source: additional`, `field: "target"` | `group_by: "field"`, `group_keys: ["target"]` |

This is what the chosen shape buys and the rejected ones could not: `trace_id` and
`span_id` carry **`selector: null`** — no log filter reaches them (`fields.rs:174-175`,
`methods.rs:341-346`) — so a selector-string vocabulary cannot name them at all.

### Rejected shapes (recorded, not re-litigated)

- **`group_by` = a single DSL selector.** Rejected: `trace_id`/`span_id` unreachable;
  `level` is **not a `Selector` at all** — the enum (`filter/parser.rs:166-179`) has no
  level arm, and `l>=` parses to `Qualifier::LevelFilter` (`:16`, built at `:311-327`);
  and gaining tuples later would change the parameter's type.
- **`group_by: {source, field}`** — structurally different and unambiguous. Rejected: a
  third vocabulary nothing else in the tree speaks.
- **A heterogeneous composite axis** — `Vec<{source, field}>`, letting `level` × `target`
  be one grouping. **Raised by the gate as a shape that should have been offered**
  (Splunk `by host, kind`; LogQL `sum by (level, app)`; Elasticsearch `composite`). It
  would not have won — it mirrors nothing in the tree, and the user's stated criterion was
  symmetry — but recording it matters because the gate also caught the asymmetry in the
  rejection above: *this* shape has the same "gaining cross-kind tuples changes the
  parameter's type" defect one axis-kind over. Step 4 inherits this vocabulary (§8), so
  the limit is written down rather than discovered later.
- **A time-bucketed axis** (`date_histogram`, `bin(5m)`, `count_over_time`). Also
  universal in this domain and absent from every spec in this family. Structurally
  different because the axis is *derived and takes an argument*, and none of the offered
  shapes has a slot for a per-axis parameter. Not offered, not deferred — simply missing.
  Recorded as a real gap for a later piece; the sentence that closed the door was "the
  enum arms are exactly the rows `logs.fields` reports", which silently rules out every
  derived axis.

---

## 3. Reply

Structs are named **`LogsProfile`** and **`LogsProfileResult`**, plus `LogGroup` and
`LevelCounts`. The names are load-bearing: `Tool::definition_name()`
(`mcp_tools.rs:181-193`) derives `LogsProfile` from `logs.profile`, and any other name
leaves `input_schema: None` and reddens
`every_tool_in_the_manifest_carries_its_parameter_schema` (`mcp_tools.rs:805-821`).

Counts are **`usize`**, matching `LogsFieldsResult` (`methods.rs:384-390`) rather than
`ProfileResult`'s `u64` (`:1946`). The log family already chose; the choice changes the
generated schema (`format: uint` vs `uint64`) and §9 makes `verify-schema` a gate.

### Top level

Carries the same evidence fields as `logs.fields`, because it describes a population and
the same question — *was this population complete?* — has to be answerable:

- `matched`, `scanned`, `buffer_total` — `scanned` is the whole ring (§4)
- `buffer_oldest_seq`, `buffer_newest_seq`, `lost_below`, `truncated`,
  `evicted_before_window`
- `grouped_by` — the axis actually used, echoed. `null` when none was asked for
- `group_keys` — echoed; `null` when none were given or an empty array normalised
- `groups` — the rows (§5.6 for what `top_n` bounds)
- `groups_total` — distinct keys **before** truncation, `__absent__` and `__overflow__`
  **included**. Follows the profile convention, not the `CollectorsDiffResult` one (C5)
- `cardinality_capped` — true when any key folded into `__overflow__`
- `levels` — counts across the whole matched set (§3.2)
- `first_seq` / `last_seq` / `first_time` / `last_time` — bounds of the matched set
- `suppressed` — the honesty channel (`methods.rs:993`), where §5.4's warnings land

With `group_by: none` (or omitted), `groups` is empty and `grouped_by`/`groups_total` are
`null`. `groups_total: 0` would read as "this query touched nothing" rather than "we did
not look" — the distinction `ProfileResult::groups_total`'s own doc draws.

### 3.0 What reconciles with what

`matched`, `levels`, `first_seq`/`last_seq` describe the **whole matched population**.
`groups` holds at most `top_n` **real** rows, plus up to two reserved rows (§5.6). So:

> The returned rows sum to `matched` **only when the real-key count is at most `top_n`.**
> `groups.len()` may therefore be as large as `top_n + 2`.

`groups_total` is what makes the shortfall visible, which is why it counts `__absent__`
and `__overflow__` like any other key. The invariant that must hold unconditionally is
over the **untruncated** group set, and §7 tests it at the projection seam, before `top_n`
applies. Asserting it on the reply would be a test that passes only on fixtures smaller
than the default — the trap `scanned` exists to avoid one level up.

### 3.1 Per group — `LogGroup`

- `key` — the value; `__absent__` or `__overflow__` for the reserved buckets; tuple
  members joined with ` / `, mirroring `group_label` (`collector/project.rs:913-932`),
  which renders a single key bare rather than as a one-element tuple
- `count`
- `levels` — the same fixed shape as the top level, scoped to this group
- `first_seq` / `first_time` / `first_exemplar`
- `last_seq` / `last_time` / `last_exemplar`

**Each end of the row is one whole record** — a seq, its timestamp, and its message. That
is why `first_time` is the timestamp *of the record at `first_seq`*, not `min(timestamp)`:
`timestamp` is emitter-supplied (`gelf/message.rs:185-195`) while `seq` is assigned at
receipt, so UDP reordering, several senders or clock skew make `max(timestamp)` and
`timestamp@max(seq)` different records. One rule, stated at both levels, and a row that
cannot describe two records at once.

Top-level `first_time`/`last_time` are `null` when `matched == 0`; a group row cannot have
them absent, since a row exists only because a record landed in it.

### 3.1.1 Value extraction is stated per axis, because `logs.fields` does not use one function

The previous revision said "reuse `logs/fields.rs`'s `render`". **That was wrong**:
`render` (`fields.rs:80-85`) handles *additional* fields only. `FieldMap::observe`
(`fields.rs:228-262`) spells every built-in by hand, and an implementation that guessed
would contradict the map one call apart:

| axis | spelling, mirroring `fields.rs` line for line |
|---|---|
| `level` | `e.level.to_string()` (`:233`) — **`Display`, not `{:?}`**; the source comment records that Debug "happened to agree, which is exactly how a spelling drifts" |
| `message` | `e.message.clone()` (`:234`) |
| `host` | `e.host.clone()`, and **empty is `__absent__`** (`:235`) — `LogEntry.host` is `String`, not `Option`, so nothing else signals absence |
| `facility` / `file` | the `Option`'s value; `None` is `__absent__` (`:238-243`) |
| `line` | `v.to_string()` on the `u32` (`:245`) |
| `trace_id` | `format!("{t:032x}")` (`:251`) — zero-padded to 32; `{t:x}` gives a different key |
| `span_id` | `format!("{s:016x}")` (`:254`) |
| additional field | `render(v)` (`:80-85`), promoted to `pub(crate)` |

§7's R-row tests this as a **cross-tool identity** rather than eight separate assertions.

### 3.1.2 A structured value is not a dimension

`ValueKind::Other`'s own doc (`methods.rs:293`) says an array or object is *"present, but
**neither a dimension** nor a number"*, and the span side excludes them outright
(`state.rs:398-400`). The previous revision argued the opposite from "family consistency"
and was simply wrong about what the family says.

There is also a correctness reason. `schemars` is built with `preserve_order`
(`crates/protocol/Cargo.toml:10`), which pulls `indexmap` into `serde_json`
(`Cargo.lock:1775`), so `Value::Object.to_string()` preserves emitter key order — a Rust
emitter logging a `HashMap` field would split one logical value across several groups
nondeterministically, and P11 would not catch it.

So: an array or object value folds to `__absent__`, mirroring `state.rs:234-236`, **and
pushes a `Suppressed` entry** naming the field and how many records it covered. Folding
silently would make `__absent__` mean two things with nothing saying so.

### 3.1.3 The accumulator key is typed, and the cap is on keys

The previous revision said three incompatible things about the cap. Settled:

```
enum MemberKey { Value(String), Absent, Overflow }
type GroupKey = Vec<MemberKey>;          // length 1 for a non-tuple axis
```

- **Typed, not `String`.** `render` returns `s.clone()` for a string value, so an emitter
  field valued literally `"__absent__"` would otherwise merge with the reserved bucket and
  the denominator would lie — the failure P3 exists to prevent, which a string key makes
  invisible. The span side avoids it by construction (a real value interns to an id
  ≥ `FIRST_REAL_ID`, `intern.rs:59`).
- **The cap counts distinct accumulator keys**, at `DEFAULT_GROUP_VALUE_CAP` (1024,
  `intern.rs:23`). Past it, a new key folds to `vec![Overflow; K]` and
  `cardinality_capped` is set. **This is not per-member interning**: the span side interns
  each member independently (`state.rs:67-71, 240`) and *can* render `svc-a / __overflow__`,
  but its bound is then 1024^K. Each log group row holds a `LevelCounts`, four bounds and
  two exemplar `String`s, accumulated under the store read lock (`memory.rs:139`) — an
  unbounded table here is precisely what `intern.rs:5` warns of.
- `state.rs:246-250` is cited for the **fold mechanism only**. Its own limit is
  `MAX_GROUP_TUPLES = 64`, a different number bounding a different thing; the previous
  revision's "the same limit the collector uses" was false.

### 3.2 `levels` is a fixed struct, not a map

`Level` has five variants and no more (C12), so `LevelCounts { trace, debug, info, warn,
error }` with every field always emitted beats a `HashMap<String, usize>`: a reader never
has to tell "key absent" from "count zero", the renderer gets fixed columns in severity
order, and the schema states the whole domain instead of `additionalProperties`.

---

## 4. The walk

`InMemoryStore::for_each_matching` (`store/memory.rs:135`) via `pipeline.for_each_log`
(`engine/pipeline.rs:209-225`), which also yields `buffer_total` and the ring bounds — so
every top-level field §3 lists has an existing source and **no new seam is needed**.

`scanned` therefore covers the entire ring rather than a `count`-limited prefix:
`recent_with_scanned` breaks at `count` (`memory.rs:99-101`, asserted by its own test at
`:354-360`) and a profile built on it would describe the newest slice while looking
authoritative.

---

## 5. Decisions, and why

### 5.1 `first_seq` / `last_seq` are min/max of `seq`, never walk order

The walk is oldest-first (C6) while every read an agent has seen is newest-first (C7) —
both orders coexist in one file (`memory.rs:91` forward, `:105` `.rev()`), so this is not
defensive. Two comparisons per record, and they cannot drift.

### 5.2 Both ends carry an exemplar — **user-decided**

The previous revision picked "newest" on a follow-up-ergonomics argument, after itself
noting both ends cost one assignment. The gate called that a tiebreak made without asking,
and pointed at the user's own B4 scar: for a recurring error the **first** occurrence
usually carries the root cause and the later ones are cascade or retry.

**Both, chosen 2026-08-03.** It extends §3.1's coupling rather than choosing a side —
each end of the row is a complete record — and answers a question neither single exemplar
can: *did this group change shape over the window?* The renderer collapses them to one
line when equal, so the two-line form is itself the signal that something moved.

Verbatim, never synthesised — `2026-08-02-log-aggregation-design.md` §4 argues this at
length: a normaliser that over-merges reports two distinct failures as one row and the
reader concludes one bug where there were two.

### 5.3 `group_by: "message"` is allowed, not refused

Over the whole buffer `message` is 4235 distinct values across 9347 records — not an axis.
Under `l>=Error` it is **6 groups over 33 records**. A blocking check that is wrong half
the time gets disabled, which is the failure mode the success-typing rule exists to avoid.
`groups_total` and `cardinality_capped` tell the truth in the bad case.

**The overlap with B4 is real and is recorded rather than resolved.** The gate noticed
that this justification and the parent spec's case for B4 rest on the *same probe over the
same filter on the same buffer* (`2026-08-03-log-aggregation-design.md:34-42`). B4 does
this properly, with a signature; `message` here is the crude version that arrives two
steps earlier. The risk that the crude version is what an agent finds and the real one
never gets built is the user's to price, and it is flagged in §10.

### 5.4 An axis member absent from every matched record is announced

C13, reproduced live: guessing an axis name returns one `__absent__` row covering 100% of
the population, with no error, indistinguishable from a real answer.

**Per axis MEMBER, not per axis.** The gate found the previous revision's hole: with
`group_keys: ["kind", "targett"]` where only the second is a typo, the *tuple* is never
wholly absent — every row is `store.reconcile / __absent__` — so nothing fired and the
reply looked like a valid two-dimensional answer. That is C13 surviving in the one shape
tuples were added for, with the whole test plan green.

Mechanism: one `present: usize` **per member**, incremented on the raw walk **before any
interning or folding**. It cannot be derived from the `__absent__` row, because an
overflow fold destroys that row (`x / __absent__` becomes `__overflow__ / __overflow__`)
at exactly the cardinality where the reader most needs it. One `Suppressed` per member
with `present == 0 && matched > 0`.

**This covers every axis, not only `field`.** On the live buffer the built-in `line`
matches 0 of 9347 records while an additional field named `line` matches 9344, so
`group_by: "line"` returns 100% `__absent__` today. That is the documented normal state of
`fi`/`ln`/`fa`, and scoping the warning to `field` would leave the best-understood
instance uncovered.

**`remedy` is `Option`, and is `None` when nothing would help.** Its own doc says
*"Absent when nothing would help"* (`methods.rs:997-999`); the previous revision promised
one unconditionally. `reason` stays strictly factual in every case, matching every other
`Suppressed` push in the tree (`project.rs:827-832`) — never a "did you mean" phrasing
when nothing was found.

`trace_id` and `span_id` are the **permanent** no-candidate case: they are closed-enum
arms, so there is no typo to have made, and the parser `.remove()`s them from
`additional_fields` (`gelf/message.rs:216-224`), so there is never a same-named additional
field to fall back on. A domain of untraced records — background jobs, startup, health
checks — is ordinary, and the honest reply there is a factual reason and no remedy.

Where a candidate *does* exist the remedy is sharp, and derivable from the same walk:

> `line` (built-in) is absent from all 9347 matched records. An additional field named
> `line` covers 99.97% — use `group_by: "field", group_keys: ["line"]`.

Holding a **single `usize` per axis member**, not a candidate map: the walk visits every
additional-field key, but retaining all of them is the unbounded name table `NAME_CAP` was
added to stop (`fields.rs:22-37`: "~10k rows and ~1.2 MB of reply").

Announced rather than refused, for the same reason as §5.3. `matched == 0` is excluded:
every axis is absent from an empty population, and warning about the axis would misdirect
a reader whose actual problem is the filter.

**Known boundary, deliberately not closed.** A near-total absence — 9999 of 10000 —
warns about nothing. Lowering the threshold would start warning about a field genuinely
present on 1% of records, which is a real signal and exactly the false positive this
design refuses elsewhere. Recorded, not fixed.

### 5.5 The cursor is rejected **before** bookmark resolution

Identical to `logs.fields` (`rpc_handler.rs:902-926`): `resolve_bookmarks` →
`cursor_read_and_advance` **auto-creates a bookmark at seq 0** for an unknown cursor name
as a side effect, so a refusal issued afterwards is honest about not advancing and
silently wrong about leaving nothing behind. The generic `parse_and_resolve_filter`
(`:856-882`) resolves unconditionally and must **not** be used here.

Empty or whitespace `filter` means *no filter*, not a parse error.

The gate's false-positive lens confirmed this check is correctly scoped, with evidence:
the rejection is AST-typed (`contains_cursor_qualifier`, `parser.rs:767-774`) rather than a
substring match, so a quoted `"c>=5"`, a regex `/c>=5/`, and a bookmark name containing
cursor syntax all parse to something else or fail earlier for an unrelated reason —
`is_valid_bookmark_token` (`:217-223`) forbids `>` and `=` in a name outright.

### 5.6 Ordering, and where the reserved buckets sit — **user-decided**

Real keys sort by `count` descending, then `key` ascending. Ties broken on the key so two
identical calls agree; leaving that to `HashMap` iteration is the reproducibility bug this
project has ruled against twice (`rank_and_truncate`, `project.rs:1254-1270`;
`fields.rs:343-348`).

**Reserved buckets sort last and are outside the `top_n` budget** (chosen 2026-08-03).
The previous revision sorted them by count like anything else, which put `__absent__` at
row 1 on 20 of 31 axes — the bucket you did not ask about, consuming a slot you did. The
gate's decisive observation: `logs.fields`, one call earlier, makes presence a **column on
every row**, never a row of its own.

So asking for 20 rows buys 20 real values, and the share goes in the rendering's header
line where it cannot be missed (§6.3). Reserved rows are still rows — `traces.profile`
renders them that way and the user chose to mirror it — they just do not compete.

---

## 6. Rendering

A dedicated renderer, `render/profile_logs.rs`, wired into `render::for_method`
(`render/mod.rs:182`). Not a `LISTS` entry: `levels` is a nested object the generic table
cannot flatten, and the level columns are derived from which levels appear.

**Why this section is load-bearing:** `crates/mcp/src/server.rs:205-228` replaces the reply
with `_display` when a renderer exists, so the agent never sees the JSON. Anything the
renderer omits does not exist for the primary consumer.

### 6.1 Every caller-derived string is flattened, clipped or not

`escape::flatten` (`render/escape.rs:14`) collapses `\n`, `\r` and the three Unicode line
breaks. Group keys, exemplars, echoed `group_keys`, and `Suppressed` `field`/`reason`/
`remedy` are all emitter- or caller-derived: **510 of 4235 distinct messages in the live
buffer contain a line break**, and one reaching a table cell splits the row.

The previous revision wrote this as "flattened *before it is clipped*" and justified it
with "every sibling renderer flattens". **The gate falsified that, from two lenses
independently**: `render/profile.rs`, `status.rs` and `trace.rs` contain no `flatten` call
at all — `profile.rs:159` takes a group `key` straight through `.as_str()` into a
fixed-width column, and `trace.rs:59` prints a caller-controlled span name the same way.
Only `blocks.rs` and the generic `table.rs`→`cell()` path reliably flatten. So the rule is
stated on its own merits, the clipped/unclipped distinction is dropped, and
`render/fields.rs` is not the outlier it was described as.

The separate `render/fields.rs` fix (below) widens to its `name` and `selector` columns
too — GELF field names are emitter-controlled (`gelf/message.rs:208-214`).

### 6.2 Clipping must not make two distinct rows render identically

Measured on the live buffer: two `Error` messages share a 70-character prefix
(`AudioMediaSystem /Services/AirZone: malformed State on AirZone/State/5`) and diverge at
character **71** — `36875849…` vs `20098633…`. They render as the same string at every
clip width up to 70. *(The previous revision said "character 52"; the grounding audit
flagged it as inconsistent with its own quoted text, and the measurement above settles it.)*

Resolution, one pass: clip to a fixed width, then **append ` #<first_seq>` to every row
whose clipped key collides with another's**. `first_seq` is unique per row — each record
lands in exactly one group, so two groups' minima cannot coincide — and it feeds
`get_log_context` directly, so the disambiguator doubles as the pointer to go look.

A width search that widens until distinct was considered and dropped: a second mechanism
for the same property, and one unlucky pair widens the column for every row.

### 6.3 The anti-bias fields must reach the rendering

Not optional decoration. The header prints `matched`, how many carry the axis, the
`__absent__` share, and **`top N of M` whenever `groups_total` exceeds the real rows
shown**, plus `cardinality_capped`, `truncated`/`evicted_before_window` and `lost_below`:

```
log profile by `kind` — 9347 matched, 1269 carry it, 8078 __absent__ (86.4%), top 8 of 18
```

A reply carrying `groups_total: 900` that rendered 20 rows and nothing else would
reproduce the parent spec §1's headline defect — *an agent sees a sample with nothing
bounding what was missed* — inside the tool built to remove it. This family has already
paid for it once: `render/fields.rs:54-71` and its test
`a_wrapped_ring_is_announced_from_the_buffer_bounds` exist because the buffer bounds were
dropped from a rendering and "the skill's own instruction became unfollowable".

**Plus the unknown-key backstop** in the shape of `render/profile.rs:219-246`, so a field
added to `LogsProfileResult` later cannot silently vanish from what the agent sees.
`render/fields.rs` has no such backstop, which is how it lost the bounds.

---

## 7. Test plan

Every row observing a whole-population invariant drives the **projection seam**
(`GroupMap::finish`) with `top_n` above the fixture's group count, per §3.0.

| # | Property | Seam | Tool / level | Catches |
|---|---|---|---|---|
| P1 | `sum(group.count) == matched` over the **untruncated** set | `GroupMap::finish` | unit | The reconciliation defect the cold reader found, off by 24 records |
| P2 | A record lacking the axis lands in `__absent__`, never dropped | fixture mixing present/absent | unit | The median case (C9) — silently dropping 86% of the population |
| P3 | `__absent__` and `__overflow__` stay distinct past the cap; `cardinality_capped` set; **and a field valued literally `"__absent__"` does not merge with the bucket** | fixture above the cap; plus one record with the literal value | unit | Conflating "lacked the field" with "folded past the cap"; and §3.1.3's untyped-key defect, which a `String` key makes invisible |
| P4 | `groups_total` counts before truncation, both reserved buckets included | more groups than `top_n` | unit | "Top 20 of 20" reading like "top 20 of 900" |
| P5 | `first_seq`/`last_seq` are min/max, not walk order | fixture whose **append order is not seq order** | unit | §5.1 |
| P6 | `first_exemplar`/`last_exemplar` come from the records at `first_seq`/`last_seq`, verbatim | **shares P5's fixture** | unit | A synthesised, cross-group, or last-visited exemplar. On a seq-ordered fixture the mutant and the original agree, so the shared fixture is what makes this test able to fail |
| P7 | `first_time`/`last_time` are the timestamps of those same records | fixture where timestamp order contradicts seq order | unit | §3.1 — two ends of a row describing different records |
| P8 | An axis **member** absent from every matched record yields one `Suppressed` naming that member — driven three ways: a `field` typo, a wholly-absent **second tuple member**, and a built-in (`group_by: "line"`) | unit | C13, and the tuple hole the gate found |
| P9 | The no-candidate case yields `remedy: None` and a factual `reason` | `group_by: "trace_id"` over untraced records | unit | §5.4 — a "did you mean" with no correct target |
| P10 | `matched == 0` yields **no** absent-axis warning | empty population | unit | Misdirecting a reader whose real problem is the filter |
| P11 | `group_by: "field"` with missing keys errors; **`group_keys: []` on any axis succeeds**; non-empty `group_keys` on a non-`field` axis errors; `>8` keys errors | RPC | `crates/core/tests/` | §2 — including the false positive the gate found on the empty array |
| P12 | A tuple joins members and keeps `__absent__` **per member** | two keys, one present one absent | unit | A partially-absent tuple folding whole |
| P13 | Two identical calls agree, including where counts tie | equal-count groups | unit | Iteration-order dependence |
| P14 | A cursor qualifier is rejected **and leaves no bookmark behind** | `c>=x`, then list bookmarks | RPC | §5.5. `logs_fields_rpc.rs:292-315` is a working template |
| P15 | `filter: ""` means no filter, not a parse error | RPC | `crates/core/tests/` | The regression the re-gate caught on `logs.fields` |
| P16 | Per-group `levels` sum to the top-level `levels`, per level, untruncated | fixture spanning several levels and groups | unit | Two numbers describing one population, disagreeing |
| P17 | Rendering flattens line breaks in keys, exemplars **and `Suppressed` text** | `\n`, `\u{2028}` in each | render unit | §6.1 |
| P18 | Two keys differing only past the clip width render distinguishably | the live `AirZone` pair | render unit | §6.2 |
| P19 | `groups_total: 900` with 20 rows renders `900`; an unknown reply key still reaches the output | render unit | §6.3 — the anti-bias fields and the backstop |
| P20 | A structured field value folds to `__absent__` **and pushes a `Suppressed`** | one record whose value is an object | unit | §3.1.2 — silently folding, and the nondeterministic split |
| P21 | `group_by: none` returns no rows and `groups_total: null`, not `0` | ungrouped | unit + RPC | "we did not look" reading as "touched nothing" |
| **R** | **Cross-tool identity:** for each built-in axis over one fixture, the set of `logs.profile` group keys equals the set of `logs.fields` `top_values[].value` for the same `(source, field)` row | unit, both projections | Every §3.1.1 row at once — level spelling, id padding, `line` stringification, empty-`host` — and it fails on drift in either direction |
| S | `group_by`'s schema enum equals what the daemon parses, both directions | `crates/core/tests/schema_matches_daemon.rs` | integration | §9 — the file exists because "six of the ten doc comments this replaced were already wrong about their own parameter" |

### Negative controls

Per mechanism, each naming the **original defect** rather than a break in new code:

- **P2** — drop records missing the axis. P2 red **and P1 red**; if P1 stays green the
  fixture has no absent records and proves nothing.
- **P5** — take first/last visited instead of min/max. P5 red, P1 green.
- **P6** — take the last-visited message. P6 red, P5 green.
- **P8** — scope the `Suppressed` push to whole axes rather than members. **Only the
  tuple arm reddens** — which is the control proving the three arms are not redundant.
- **P12** — fold the whole tuple to `__absent__` when any member is missing. P12 red,
  P2 green.
- **P17** — remove the `flatten` call. P17 red. Reproduces the defect at *its* site in
  `render/fields.rs`, not in new code.
- **P18** — clip at a fixed width with no collision check. P18 red, P17 green.
- **P19** — drop `groups_total` from the renderer. P19 red; the backstop half must then
  be reddened by a *different* unknown key, or the two halves are one test.
- **R** — change `level` to `{:?}`. R red while every within-tool test stays green, which
  is the point: nothing but a cross-tool assertion can see this drift.

Every assertion behind a loop or filter carries a count guard proving the body ran.

---

## 8. What step 4 adds, and why nothing here blocks it

The extraction stage attaches as an optional `values` parameter and an optional `values`
block on each `LogGroup` and at top level. Both are additive.

The axis vocabulary is what step 4 inherits, and it is why this piece went first: an
extracted value has to be *named* to be grouped by, and `(group_by, group_keys)` is the
naming scheme it will extend. §2 records the limit the gate identified — cross-kind
grouping needs `group_by` to stop being a string — so step 4 inherits the constraint
knowingly.

The same applies to spans, which cannot sum their own attributes today either:
`render_attribute` (`collector/state.rs:393-401`) turns every number into a `String` on
its way into the interner.

---

## 9. Definition of done

| Artifact | Needs |
|---|---|
| `skill/logmon.md` | The `logs.fields` → `logs.profile` pairing as one workflow; that `__absent__` is normal and often the largest bucket; that an absent-axis warning means a typo or a wrong spelling, not an empty buffer |
| `README.md` | A tool-table row, and `logs.profile` filled into the three-questions table of the parent spec §1 |
| `docs/medium-article.md` | Why the reserved buckets are the honest part: a distribution that does not account for its own population is a chart that lies |

Code-side gates, each one a place a feature in this family lost a step:

- `LogsProfile` / `LogsProfileResult` / `LogGroup` / `LevelCounts` in
  `protocol/src/methods.rs` — the first two names are derived, not free (§3)
- A `Tool` entry in `mcp_tools.rs`, and the `TOOLS.len()` pin at
  **`mcp_tools.rs:1095`** 48 → 49. *(Not `capability_skew.rs` — the previous revision said
  so and two gate lenses caught it. `capability_skew.rs:171` asserts `probed ==
  TOOLS.len()`, which is derived and needs no edit.)*
- **A row in `crates/core/tests/schema_matches_daemon.rs`** for the `group_by` enum. This
  forces the Rust shape: an `ALL` const, an exhaustive `as_str`, a `parse`, and
  `accepted()` via `crate::rejection::one_of`, mirroring `GroupBy` (`project.rs:61-105`)
- All four new types registered in `crates/xtask/src/main.rs`'s hand-maintained map
  (`:63`) — `verify-schema` re-runs that map against itself, so an omission is invisible to
  it; the re-gate found `FieldSource` missing last round
- `cargo xtask gen-schema` + `verify-schema`
- A `render::for_method` arm (`render/mod.rs:182`)
- The MCP tool **description** — the copy closest to the agent, and the one that went
  stale last round

---

## 10. The gate — what it found, and where each finding landed

Four lenses, 25 actionable findings, five convergent. The finding count is the diagnosis:
**the design self-review did its job on citations and not on buildability.** Round 1 left
the document and checked every reference; it never once asked *what would I have to invent
to build this?* — and every blocker was an answer to that question.

**Resolved in this revision:** the cap's three contradictory readings (§3.1.3); value
extraction assuming one function where `logs.fields` hand-spells eight (§3.1.1); structured
values as dimensions, against the protocol's own doc (§3.1.2); the absent-axis hole at a
tuple member (§5.4); `remedy` promised unconditionally against its own contract (§5.4);
`group_keys: []` refused despite a tested convention (§2); the missing tuple-width cap
(§2); `first_time`/`last_time` unruled (§3.1); the anti-bias fields never required to
render (§6.3); "every sibling renderer flattens", false (§6.1); the `capability_skew.rs`
mis-attribution and the missing `schema_matches_daemon.rs` row (§9); struct names and
integer widths (§3); C11's own `line` ambiguity (§1); §6.2's character 52 → 71.

**Decided by the user, recorded so they are not re-raised:** the hard error on misplaced
`group_keys` (§2), both exemplars (§5.2), reserved buckets last and outside `top_n` (§5.6).

**Recorded, not resolved:** the composite and time-bucketed shapes that should have been
offered (§2); the `message`-axis overlap with B4 (§5.3); the near-total-absence threshold
(§5.4); B7's relationship to `group_by: "host"` — `logs.profile` serves its per-source
counts through another door but not its rate or receiver liveness, so B7 is not superseded.

**Found by the gate, outside this spec — a real shipped defect.**
`gelf/message.rs:216-224` calls `additional_fields.remove("trace_id")` **before** checking
the value parses as hex, so a `_trace_id` that is not lowercase hex — a UUID, a decimal id
— is removed and then dropped when `from_str_radix` fails. Not promoted, not additional,
no `GelfParseError` variant, no test. Same for `_span_id`. Filed separately; it is why
§5.4 treats those two axes as permanently no-candidate.

**Confirmed sound, with evidence** (two-way evidence matters as much as findings): the
walk and its counts (§4); the cursor rejection, shown AST-typed rather than substring-matched
against four attack shapes (§5.5); the axis enum's completeness, checked in both
directions against `BUILTINS`; §3.0's decision to assert invariants at the projection seam;
§6.2's uniqueness premise for `first_seq`; and 27 of 30 citations.
