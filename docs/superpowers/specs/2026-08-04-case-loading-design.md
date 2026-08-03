# Case loading — design

**Tier: T2.** Novel: no reader for the case format exists in-repo, and the change
mints a consumer for a persisted contract that outlives the code.

Basis: `2026-08-03-case-loading-proposal.md`. That document is a proposal; this one
is the design. Where the two disagree, this one is later and wins — §2 records every
place they disagree and why.

**Scope.** `load_case` and the writer fix its duty-0 probe forced. **Not** in scope:
unattended daemon-side case writing (proposal §6), which composes with this but does
not depend on it, and replay (proposal §8), which stays deliberately foreclosed.

---

## 1. Duty 0 — the premises, checked against a running broker

Every §4 claim of the proposal was re-verified. Three were checked by *producing real
case files* from an isolated broker (`LOGMON_CONFIG_DIR`, ports 22201/24317/24318)
rather than by reading the writer's source, because the loader's premises are almost
entirely assertions about what the writer emits.

| # | Claim | Verdict |
|---|---|---|
| C1 | Three files share one stem | **confirmed** — `cases/naming.rs:95` (proposal cited 98–99; line drift only) |
| C2 | Evidence is full-fidelity, not a projection | **confirmed** — `rpc_handler.rs:2072` passes `logs.iter()` / `spans.iter()` through generic `write_jsonl<T: Serialize>` |
| C3 | `LogEntry` derives `Deserialize` | **confirmed** — `gelf/message.rs:90` |
| C4 | *(unclaimed by the proposal)* `SpanEntry` too | **confirmed** — `span/types.rs:40`, with symmetric hex codecs for `trace_id`/`span_id`/`parent_span_id` |
| C5 | Every JSONL carries `FORMAT_VERSION` first | **confirmed** — real file line 1 is `{"logmon_format":1,"kind":"logdata"}` |
| C6 | A receiverless domain is possible | **confirmed** — created one live with all four ports `0` |
| C7 | Provenance rides along with a case | **confirmed** — real front-matter shows `provenance: {core: "3 of 3", missing: []}` |
| C8 | **The round trip is "plausible"** | **UPGRADED to proven.** 9 real log records and 3 real spans deserialized into the store's own types and re-serialized with no content loss |

**C8 qualifier — lossless is not byte-identical.** `additional_fields` and
`attributes` are `HashMap`s, so key order varies across a round trip. Nothing may
checksum or byte-diff a case file to test equivalence. This is also why §5.4 refuses
re-export rather than treating it as idempotent.

### 1.1 What the probe falsified

**F1 — the writer can capture ZERO spans for the trace the case is about, then
report that nothing is missing.** Reproduced: logs took seq 1001–1006, spans
1007–1009, and a case anchored on the error log produced `spandata: {records: 0}`.

Spans are selected by the **log-derived** seq range (`rpc_handler.rs:1987`), while
`short_after` and the window's `to` come from `context_by_seq` over *logs only*
(`rpc_handler.rs:1956`). One `SeqCounter` numbers both stores
(`domain.rs:176-180` — the counter is cloned into the pipeline and moved into the
span store), and OTLP exports a span when it **ends**, so a slow operation's spans
land *above* the last log about it.

The document then emits, from `document.rs:531`:

> "After the window: seq 1006 was the newest record stored when the capture was
> taken, so the shortfall after the anchor is history that had not happened yet. A
> ring evicts from the bottom, never the top — nothing was lost here."

**False.** Seqs 1007–1009 were stored at 21:39:46; the capture ran at 21:41:21 — 95
seconds later. Two paragraphs earlier the same document says *"one counter numbers
both logs and spans, so a span consumes a seq no log will ever occupy"*. It knows,
then forgets.

Fixed by §4. **This is the finding that most justifies duty 0 on this feature**: a
loader designed against the writer as it behaves today would faithfully load cases
whose evidence is systematically incomplete, and the document would tell the reader
it was complete.

**F2 — `anchor.trace_id` resolves through LOGS, not spans.** A trace-anchored case
fails with *"no stored log entry carries trace id X"* unless a log carries it.
Recorded, not fixed: it is a real limitation for services whose logs lack trace ids,
but it is orthogonal to loading.

**F3 — the front-matter escaper is sound.** `yaml_str`
(`collector/document.rs:1836`) escapes `\\`, `\"`, `\n`, `\r`, `\t`, all C0 + DEL as
`\xNN`, and NEL/LS/PS as `\N`/`\L`/`\P`. Verified against hostile input; a U+2028 that
entered the probe string by accident came out as `\L` rather than breaking the scalar.
**This is what makes §3.2 safe**, and it is the reason a hand-rolled parser is
defensible rather than reckless.

**F4 — the CLI cannot express array-of-object parameters.** `domain-data update
--entries '{"path":...}'` fails with *"entries[0] is missing path"*: the array flag
coerces each repeat against the `items` schema and an object item falls through to
`Value::String` (`mcp/src/cli/generic.rs:192-216`), while `insert_nested` only ever
builds nested objects (`:274`). Affects `domain_data.update` and `cases.create --data`
— the provenance surfaces. **Out of scope; file as an issue.** Not fixed here because
it is CLI grammar, not case loading, and a fix belongs with its own tests.

---

## 2. Decisions, and where the proposal is superseded

| # | Decision | Source |
|---|---|---|
| D1 | `load_case` takes the **case document** (`.md`); its front-matter carries the pointers to both data files | user, 2026-08-04. Sharpens proposal Q7 |
| D2 | The case's registry — all non-reserved facts — is restored into the domain | user, 2026-08-04 |
| D3 | The domain adopts the case's **recorded incarnation** rather than minting one | design; preserved seqs are meaningless without the era they belong to |
| D4 | **Clock frozen at `captured_at`** for `idle`/`stale` and registry expiry; **real clock** for reader-side actions | user, 2026-08-04. Answers proposal Q4 |
| D5 | `load_case` **creates** the domain; loading into an occupied postmortem domain is not expressible | user, 2026-08-04. **Supersedes proposal Q2** |
| D6 | `create_case` from a postmortem domain **refuses** | user, 2026-08-04. Answers proposal Q5 |
| D7 | The writer's window is derived from **both stores** | user, 2026-08-04, following F1 |
| D8 | The loader is **daemon-side** | `rpc_handler.rs:1885` — a client-side loader would push the archive back through the model's context, the outcome the document/logdata split exists to prevent |
| D9 | Front-matter parsed by a **hand-rolled parser + serde type + round-trip property test** | user, 2026-08-04. Answers proposal Q6 |
| D10 | Version policy: **accept older, refuse newer** | two in-repo precedents, §3.3 |

### 2.1 D5 corrects a premise, not just an answer

The question was framed as "refuse or allow a second case into an occupied domain."
The user's answer questioned the frame: a postmortem domain is *constituted by* one
case.

The stated reason was that buffer sizes come from the case's registry values. **That
premise is false** — `log_buffer_size` appears in `cases/` only as advice prose
(`document.rs:711`), never as a captured value, and buffer size lives in domain config
(`domain.rs:111`), which the case registry copy explicitly excludes along with all of
`/logmon/*` (`rpc_handler.rs:2270`).

The conclusion holds on stronger ground. **On a sealed domain, buffer capacity has no
behaviour** — nothing will ever arrive to fill it, so capacity beyond the record count
is unobservable. What makes the domain unreusable is that it carries *one* case's
`captured_at` as its frozen clock, *one* incarnation, *one* registry, *one* seq range.
A second case would put two capture times in one domain and the frozen clock would
have no coherent value.

Recorded because the distinction changes the design: no "second load" refusal surface
is needed. The only collision left is an ordinary name collision.

---

## 3. The design

### 3.1 The seam that does not exist, and the shape that avoids creating it

**There is no path today for inserting a record with a seq already assigned.**
`SpanStore::insert` overwrites it — `span.seq = self.seq_counter.next()`
(`span/store.rs:55`) — and `InMemoryStore` exposes no public insert at all; logs
arrive through the pipeline's processor task.

The obvious move is to add `insert_preserving_seq` to both stores. **This design
refuses that**, because it creates a live mutation path that every future caller
inherits, on the exact surface the sealed-domain rule exists to close. It would also
raise the tier: a public seq-overriding insert on a shared store is core-engine
surface where a bug corrupts state.

**Instead the loader BUILDS the stores populated and hands them to `Domain`.**

```
InMemoryStore::from_records(capacity, records, lost_below)                 // new, private
LogPipeline::from_records(capacity, seq_counter, records, lost_below)      // new
SpanStore::from_records(capacity, seq_counter, records, lost_below)        // new
Domain::from_parts(config, pipeline, span_store, bookmarks, metrics)       // EXISTS, domain.rs:143
```

**`Domain::from_parts` takes an `Arc<LogPipeline>`, not a store** — `LogPipeline`
owns `store: InMemoryStore` by value (`engine/pipeline.rs:100`), so the log-side
constructor is on the *pipeline*, and `InMemoryStore::from_records` stays private
beneath it. Getting this backwards was a defect in this design's first draft, caught
by opening the constructor instead of trusting the name.

The `from_records` constructors are **constructors** — reachable only before the
domain is published, never on a live store. **Sealing becomes structural rather than
enforced**: there is no path to add a record after construction, so it cannot be
forgotten for a tool added later.

Each fills `entries`, `seq_set`, `trace_index`, sets `total_stored` /
`total_received` to the record count, and sets `lost_below` as below.

**`lost_below` = `seq_range.from`, derived, no format change.** A loaded domain
genuinely cannot speak for anything below `from` — it does not have those records.
That is true regardless of *why*, so the correct value follows from front-matter that
already exists. Setting `0` would claim the axis starts at the beginning of time,
which is the completeness lie `lost_below` was built to prevent
(`store/memory.rs:31-38`).

### 3.1.1 The epoch log — the part that does not survive a naive load

`LogPipeline` also owns an `EpochLog`, which records *what storage policy governed
each stretch of the seq axis* and is what `logs.export` consults to say how much of a
window it can vouch for.

**It is written only by `observe()`, called from the log processor during ingest.** A
loaded domain never ingests, so the log stays **empty** — and
`EpochLog::coverage` returns `EpochCoverage::default()` for an empty log, i.e.
`covered: false`, documented as *"silence is not evidence of an unfiltered store"*
(`engine/epoch.rs:126-130`).

So a naive load produces a domain that reports **`cannot_verify` over the very window
whose document says `verdict: complete`** — the reconstruction contradicting the case
it was built from. Nothing would fail; the answer would just be needlessly weaker than
the evidence supports.

**Two coupled decisions fix it, and they must agree:**

1. **Seed the seq counter at `from`, not `to + 1`.** `EpochLog::new(origin_seq)` is
   handed `seq_counter.current()` at pipeline construction, and the first `observe`
   opens its epoch **at the origin, not at the seq observed** (`epoch.rs:186` —
   `None => self.origin_seq`). Seeding at `to + 1` makes `coverage(from, to)` compute
   `covered = from >= to + 1` → false.
2. **Seed the log with one `observe(from, unfiltered)` when the case's verdict is
   `complete`**, and leave it empty otherwise.

This works because `complete` is *defined* as "these seqs lay wholly inside one
unfiltered epoch, nothing was evicted from it" — exactly one unfiltered epoch's worth
of information. Any other verdict leaves the log empty, and `cannot_verify` is then
the honest answer for a capture that could not vouch for itself.

**Seeding the counter below the loaded records is safe *only because* sealing is
structural.** If §3.1 had instead added a public insert-with-seq, this seeding would
be a collision waiting to happen. The two decisions hold each other up, and neither
may be changed without revisiting the other.

### 3.2 Parsing the front-matter (D9)

New module `crates/core/src/cases/read.rs`, beside `write.rs`, so the format contract
has one home rather than a reader and a writer that can drift.

One serde type, `FrontMatter`, with the 14 keys the emitter produces. The parser is
hand-rolled because the front-matter is a **fixed 14-key schema, not arbitrary YAML** —
a general parser's generality is a liability here, since it would accept documents the
emitter can never produce, letting a corrupt file parse into something plausible rather
than failing loudly. No YAML crate exists in the workspace (zero matches in
`Cargo.lock`, direct or transitive); everything the broker persists is JSON.

**The un-escaper is the risky half and gets a property test**, not an example test:

```
for all s: unescape(yaml_str(s)) == s
```

This is what pins emitter/parser drift, and it is stronger evidence than a third-party
parser's reputation. `yaml_str` is `pub(crate)` in `collector/document.rs` — the test
lives where it can reach both halves.

**Unknown keys are ignored, missing required keys are an error.** Forward
compatibility in the additive direction, matching the collector path's documented
discipline (`collector/persist.rs:184`).

### 3.3 Version policy (D10)

Two in-repo precedents, identical policy — accept older, refuse newer:

| Reader | Check |
|---|---|
| `domain_data` | `p.format_version <= FORMAT_VERSION` (`domain_data/persist.rs:174`) |
| collectors | `if file.version > FORMAT_VERSION` → error naming both (`collector/persist.rs:453`) |

Case loading adopts it. **Stated explicitly because strict equality is the easy
default and it would orphan every archived case on the first bump**, breaking the
"readable years later" promise `FORMAT_VERSION` exists to make.

Checked in **three places**, and all three must agree: the document's
`logmon_format`, and each JSONL header's `logmon_format`. A disagreement between the
three files of one case is a corrupt case, not a version question, and says so.

### 3.4 What the loaded domain carries

| From front-matter | Becomes |
|---|---|
| `logdata.records`, `spandata.records` | `log_buffer_size`, `span_buffer_size` |
| `seq_range.from` | `lost_below` on both stores; the seq counter's seed; the epoch log's origin (§3.1.1) |
| `verdict` | one seeded epoch when `complete`, empty log otherwise (§3.1.1) |
| `captured_at` | the domain's **frozen clock** (§3.5) |
| `incarnation` | `set_reserved("/logmon/incarnation", …)` (D3) |
| `anchor.seq`, `anchor.label` | a bookmark, so loading lands the reader on the record the case is about (§3.6) |
| `verdict`, `clamped`, `requested_*_missing` | surfaced by `get_status` (§3.7) |
| the document's `registry` facts | restored as `domain_data` (D2) |

Receivers: all four ports `0`. Verified possible (C6).

### 3.5 The frozen clock (D4)

Measured before designing: **the render layer never consults the wall clock** — zero
`Utc::now()` in `crates/core/src/render/`. Of 47 wall-clock sites in core, only two
mechanisms would misreport a postmortem domain:

| Site | Computes |
|---|---|
| `domain_to_info` (`rpc_handler.rs:34`) | `idle_secs` (`:51`) and `stale` (`:70`) |
| `rpc_handler.rs:647, 708, 773` | the `now` passed to the registry for `is_expired` |

Everything else is ingest (dead on a sealed domain), collector operations (refused,
§5), or test setup. **So no clock abstraction is threaded through the codebase**: the
domain carries `captured_at` and those two mechanisms consult it.

**The rule, stated so it cannot be got backwards:** the frozen clock governs *facts
about the captured system*. It does **not** govern *the reader's own actions*. A
bookmark placed today while reading a three-month-old case really was created today
(`store/bookmarks.rs:182`), and session `last_seen_secs_ago` is about the live
connection. Getting this inverted would make the reader's navigation history look like
it happened at capture time.

**The frozen clock is a lie by omission unless the domain announces itself.** §3.7 is
therefore not optional decoration; it is what makes D4 honest.

### 3.6 Landing on the anchor

The front-matter carries `anchor: {kind, value, seq, at}` — *the* record the case is
about — and bookmarks stay available on a sealed domain. Loading materialises it as a
bookmark named from `anchor.label`.

**Marked as a design proposal rather than a settled decision.** It exploits two
existing decisions and costs little, but it was not explicitly approved; strike it and
nothing else in this design changes.

### 3.7 `get_status` on a postmortem domain

Announces three things it does not announce today: that the domain is **postmortem**,
its **`captured_at`** (with the real elapsed time as an absolute, so the frozen clock
cannot mislead), and the capture's **verdict** — `complete`, or truncated with the
shortfall named.

The verdict matters as much as the clock: `verdict`, `clamped`, and
`requested_before_missing` / `requested_after_missing` say whether the evidence is
whole. Without them a reader concludes "no errors before X" when the window was merely
short — a wrong answer from a query that ran fine.

**Open, to check during implementation rather than assert here:** the profile tools
already have a `suppressed` vocabulary for *"I could not measure that"*. Whether it
fits this is worth checking; this design does **not** claim it does. If it does not,
the fields go on `DomainInfo` directly.

---

## 4. The writer fix (D7)

`before`/`after` count records in the **shared seq space** rather than log records, so
one interval still describes both files — preserving the property the current code was
built for (*"the two files describe one interval by construction rather than by two
window parameters that could disagree"*) while fixing the omission F1 found.

Consequences:

- `w.to` becomes the newest record across both stores, which makes the
  `document.rs:531` sentence **true** rather than needing separate rewording. One fix,
  two defects.
- `short_before` / `short_after` are computed against the merged record run.
- A case anchored on an error log now contains the spans of the operation that failed.

**Implementation note, not yet decided:** the merged window needs the span store's
seqs in the neighbourhood of the anchor without enumerating the whole ring.
`SpanStore::context_by_seq` already exists (`span/store.rs:236`) and is the natural
seam. Confirm it is bounded before relying on it.

**This ships as its own commit, before the loader**, with its own test and control.
It is a behaviour change to a shipped tool and should be revertable independently.

---

## 5. The refusal surface

Sealing is structural (§3.1) — no code path can add a record. The refusals below are
about *operations that would be inert*, which is a different failure: accepted and
silently doing nothing.

| Operation | Why it errors |
|---|---|
| `add_collector` | Forward-facing. Armed here it reports zero forever, which reads as *"I measured and nothing happened"* rather than *"I could not measure."* Refusal points at `profile_traces` |
| `add_trigger` | Same shape, quieter: an armed trigger that cannot fire looks like an all-clear |
| `add_filter` | Filters shape what gets *stored* at ingest; with no ingest the user believes the domain was narrowed when it was not |
| `clear_logs` / `clear_domain` | The live buffer refills; this one cannot. Deleting the domain is the honest way to discard it |
| `create_case` | D6 — re-export. Refusal names the occupying case |

**One check, not five.** A `postmortem: Option<PostmortemInfo>` on `Domain`, tested in
one place that every mutating handler passes through. Five scattered checks would mean
the sixth tool added later inherits nothing — the enumerate-today's-tools failure mode.

Bookmarks and cursors stay: read-side navigation is how a reader moves between the
document's narrative and its evidence.

---

## 6. Test table

Every row names the seam it drives and the level it observes, because a property with
neither is an aspiration.

| # | Property | Test | Seam | Level — and can the failure be seen there? |
|---|---|---|---|---|
| T1 | Front-matter round-trips | `for all s: unescape(yaml_str(s)) == s` | `yaml_str` / `unescape` | unit, property. **The escaping is the risky half; this is the test that makes D9 defensible** |
| T2 | A real case loads | fixture case → `load_case` → assert seqs, counts, levels | `cases::read::load` | integration (`--features test-support`) |
| T3 | **Seqs are preserved** | loaded record seqs equal the file's | store contents after construction | integration. A unit test of `from_records` cannot see that the *loader* passed the right records |
| T4 | Newer format refuses | doctor a fixture to `logmon_format: 2` | version check | unit — error names both versions |
| T5 | Three-way version disagreement | `.md` says 1, `.logdata.jsonl` says 2 | version check | unit — reports corrupt, not version |
| T6 | Frozen clock | `get_status` on a loaded 3-month-old case | `domain_to_info` | integration. **Unit-testing the clock helper cannot see that `domain_to_info` consults it** |
| T7 | Reader actions use the REAL clock | bookmark on a loaded case has today's `created_at` | `BookmarkStore` | integration — the inversion in §3.5 |
| T8 | Registry restored, expiry frozen | a fact with a TTL reads fresh as at capture | `domain_data` get | integration |
| T9 | Incarnation adopted | loaded domain's `/logmon/incarnation` equals the file's | `set_reserved` | integration — **negative control: without D3 it mints its own** |
| T10 | Each refusal errors | one per §5 row | the single postmortem check | integration. **Negative control: remove the check, every row must go red** |
| T11 | Truncated capture stays visible | load a case with `verdict != complete`; status says so | `get_status` | integration |
| T12 | **Writer: window covers both stores** | logs then spans, anchor on last log, assert spans captured | `cases.create` | integration. **This is F1's regression test — it must go RED under the current code** |
| T13 | Writer: the sentence is true | `w.to` is the newest record across both stores | document render | unit |
| T14 | **A loaded case does not weaken its own verdict** | load a `complete` case; `logs.export` over `[from, to]` reports `complete`, not `cannot_verify` | `EpochLog::coverage` via the export path | integration. **A unit test of the seeding cannot see that the export path consults the log** — and this is the §3.1.1 defect's regression test, so it must go RED without both couplings |

**T12 and T10 are the two that must be mutation-verified by the author at the moment
of writing** — both are fix tests, written by someone who already believes the fix is
right. Revert the fix, confirm red, restore.

**Deliberately untested:** the `HashMap` ordering of `additional_fields` across a round
trip (C8). It is not stable and no test should assert it is.

---

## 7. Docs surface

Walked from `docs/process/docs-surface.md`, not from the diff.

| Surface | Owed |
|---|---|
| `README.md` | tool-table row; **tool count must equal `mcp_tools::TOOLS.len()`** (stale twice before) |
| `skill/logmon.md` | when to reach for it, call shape, traps. `include_str!`'d — **promote before `cargo install`** |
| `CHANGELOG.md` | `Unreleased → Added` for `load_case`, `Fixed` for the writer window |
| `docs/medium-article.md` | **yes** — case loading changes an argument the article makes about what a case document is for |
| `crates/mcp/README.md` | CLI grammar for the new verb |
| `crates/sdk/README.md` | check; likely nothing |

Every pasted command and rendering must be **executed** against an isolated broker,
not reconstructed.

---

## 8. Deliberately not done

- **Replay** — proposal §8. Sealing forecloses *"would this trigger have caught it?"*.
  Whoever wants it builds replay; they do not un-seal this.
- **Unattended case writing** — proposal §6. Its own design.
- **Cross-domain diff.** The stated payoff is *"compare it to the current value"*, and
  loading gives two domains and two calls to eyeball. `diff_collectors` shows the repo
  has a diff idiom. **The user story is not finished without something like it** —
  recorded here so that is a known gap rather than a discovery.
- **F2** — trace anchoring through logs only. Real limitation, orthogonal.
- **F4** — the CLI's array-of-object gap. File as an issue.
