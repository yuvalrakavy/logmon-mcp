# Case loading — design

**Tier: T2.** Novel: no reader for the case format exists in-repo, and the change
mints a consumer for a persisted contract that outlives the code.

Basis: `2026-08-03-case-loading-proposal.md`. That document is a proposal; this one
is the design. Where the two disagree, this one is later and wins — §2 records every
place and why.

**Revision 2, after a four-lens design gate.** Revision 1's central architectural
argument rested on a false premise and its seq-axis arithmetic was wrong in two
opposite directions; §10 records every finding and where it landed. **The gate did
not polish this design, it redirected it** — read §3.1 before anything else if you
knew revision 1.

**Scope.** `load_case` and the writer fix its duty-0 probe forced. **Not** in scope:
unattended daemon-side case writing (proposal §6), which composes with this but does
not depend on it, and replay (proposal §8), which stays deliberately foreclosed.

---

## 1. Duty 0 — the premises, checked against a running broker

Every §4 claim of the proposal was re-verified. Three were checked by *producing real
case files* from an isolated broker (`LOGMON_CONFIG_DIR`, GELF 22201 / OTLP 24317 /
24318) rather than by reading the writer's source, because the loader's premises are
almost entirely assertions about what the writer emits. Those files are now committed
as fixtures under `crates/core/tests/fixtures/cases/`.

| # | Claim | Verdict |
|---|---|---|
| C1 | Three files share one stem | **confirmed** — `cases/naming.rs:95` |
| C2 | Evidence is full-fidelity, not a projection | **confirmed** — `rpc_handler.rs:2072` passes `logs.iter()` / `spans.iter()` through generic `write_jsonl<T: Serialize>` |
| C3 | `LogEntry` derives `Deserialize` | **confirmed** — `gelf/message.rs:90` |
| C4 | *(unclaimed by the proposal)* `SpanEntry` too | **confirmed** — `span/types.rs:40`, symmetric hex codecs at `:43-45` |
| C5 | Every JSONL carries `FORMAT_VERSION` first | **confirmed** — real line 1 is `{"logmon_format":1,"kind":"logdata"}` |
| C6 | A receiverless domain is possible | **confirmed** — created one live. (Three ports, not four: GELF's one port carries UDP+TCP) |
| C7 | Provenance rides along with a case | **confirmed as a RENDERING, not as data** — see §3.4.1, which is where revision 1 was most wrong |
| C8 | **The round trip is "plausible"** | **UPGRADED to proven.** 9 real log records and 3 real spans deserialized into the store's own types and re-serialized with no content loss |

**C8 qualifier — lossless is not byte-identical.** `additional_fields` and
`attributes` are `HashMap`s, so key order varies across a round trip. Nothing may
checksum or byte-diff a case file to test equivalence.

### 1.1 What the probe falsified

**F1 — the writer can capture ZERO spans for the trace the case is about, then
report that nothing is missing.** Reproduced: logs took seq 1001–1006, spans
1007–1009, and a case anchored on the error log produced `spandata: {records: 0}`.
Preserved as the fixture `checkout-hang-260803-214121`.

Spans are selected by the **log-derived** seq range (`rpc_handler.rs:1987`), while
`short_after` and the window's `to` come from `context_by_seq` over *logs only*
(`rpc_handler.rs:1956`). One `SeqCounter` numbers both stores (`domain.rs:176-181`),
and OTLP exports a span when it **ends**, so a slow operation's spans land *above*
the last log about it.

The document then emits, from `document.rs:531`:

> "After the window: seq 1006 was the newest record stored when the capture was
> taken, so the shortfall after the anchor is history that had not happened yet. A
> ring evicts from the bottom, never the top — nothing was lost here."

**False.** Seqs 1007–1009 were stored at 21:39:46; the capture ran at 21:41:21 — 95
seconds later. Two paragraphs earlier the same document says *"one counter numbers
both logs and spans"*. It knows, then forgets. Fixed by §4.

**F2 — `anchor.trace_id` resolves through LOGS, not spans.** Recorded, not fixed:
orthogonal to loading.

**F3 — the front-matter escaper is sound.** `yaml_str` (`collector/document.rs:1836`)
handles `\\`, `\"`, `\n`, `\r`, `\t`, all C0 + DEL as `\xNN`, and NEL/LS/PS as
`\N`/`\L`/`\P`. Verified against hostile input. **This is what makes §3.2 safe** — but
see §3.3.1: revision 1 leaned on it for a guarantee it does not provide.

**F4 — the CLI cannot express array-of-object parameters.** Out of scope; file it.

**Two numbers were recounted by the gate and are correct**: zero `Utc::now()` under
`crates/core/src/render/`, and exactly 47 in `crates/core/src/`.

---

## 2. Decisions

| # | Decision | Source |
|---|---|---|
| D1 | `load_case` takes the **case document** (`.md`) | user, 2026-08-04 |
| D2 | The case's registry is restored into the domain | user, 2026-08-04. **Requires a format addition — see §3.4.1. Needs sign-off.** |
| D3 | The domain adopts the case's **recorded incarnation** | design |
| D4 | **Clock frozen at `captured_at`** for facts about the captured system; **real clock** for the reader's own actions | user, 2026-08-04 |
| D5 | `load_case` **creates** the domain | user, 2026-08-04 |
| D6 | `create_case` from a postmortem domain **refuses** | user, 2026-08-04 |
| D7 | The writer's window is derived from **both stores** | user, 2026-08-04 |
| D8 | The loader is **daemon-side** | `rpc_handler.rs:1885` |
| D9 | Front matter parsed by a hand-rolled parser + round-trip tests | user, 2026-08-04. **Built and green — `cases/read.rs`** |
| D10 | Version policy: **accept older, refuse newer** | `domain_data/persist.rs:174`, `collector/persist.rs:453` |

### 2.1 D5's premise was false; its conclusion holds

The stated reason was that buffer sizes come from the case's registry values. **False**
— `log_buffer_size` appears in `cases/` only as advice prose (`document.rs:711`), and
buffer size lives in domain config (`domain.rs:111`), which the case registry copy
excludes along with all of `/logmon/*` (`rpc_handler.rs:2275`).

The conclusion holds on stronger ground: a postmortem domain carries *one* case's
`captured_at`, incarnation, registry and seq range. A second case would put two capture
times in one domain and the frozen clock would have no coherent value.

**But the gate found the collision is NOT ordinary** (§3.6): the domain_data file is
keyed by name and outlives the domain.

---

## 3. The design

### 3.1 Sealing is ENFORCED, not structural — revision 1 was wrong

Revision 1 argued: no preserved-seq insertion path exists, so building the stores in
constructors makes sealing structural. **Both halves are false, and the gate found it
three times independently.**

```rust
// store/memory.rs:169    — takes the entry's OWN seq, never reassigns
fn append(&self, entry: LogEntry) { ... let seq = entry.seq; ... }
// store/traits.rs:12     — pub trait LogStore { fn append(&self, entry: LogEntry); }
// engine/pipeline.rs:157 — pub fn append_to_store(&self, entry: LogEntry)
// daemon/domain.rs:124   — pub pipeline, pub span_store
```

So `d.pipeline.append_to_store(entry_with_its_own_seq)` is one public call away, today,
from any handler — and `crates/core/tests/pipeline.rs:59` already does exactly that.
There is nothing structural to lean on.

**What survives, and why.** Building the stores in constructors is still right, but for
a different reason: `append` sets `total_received`/`total_stored` by increment and
leaves `lost_below` at `0`, and nothing on that path can seed the epoch log. The
constructors are about **fidelity**, not sealing.

**Sealing is therefore a check, and the check needs a test that finds the tool nobody
added yet** — see §5, which is a rewrite.

### 3.2 The reader (built)

`crates/core/src/cases/read.rs`, beside `write.rs`. Hand-rolled parser for the 14-key
front matter, with a `FrontMatter` type and errors that name the key at fault.

Pinned by three test shapes: a round trip through the **real** emitter with hostile
values; exhaustive escape coverage — every character `yaml_str` branches on, then every
ordered **pair** (18k combinations, because a pair catches a scanner that consumes one
character too many and a single character cannot); and a strictness suite refusing
escapes the emitter never produces.

**The gate's T1 criticism was correct and is already addressed in the built code**: the
round-trip property quantifies over the emitter's *image*, so it cannot catch an
over-permissive un-escaper, and it says nothing about the **tokenizer** — where a `}`
inside a quoted value decides whether `logdata.records` reads as 9 or 999. Both have
dedicated tests.

### 3.3 Version policy and the checks around it

Accept older, refuse newer, matching `domain_data/persist.rs:174` and
`collector/persist.rs:453`. Strict equality would orphan every archived case on the
first bump. Checked **before anything else is interpreted**, with a test pinning the
order.

Three files carry a version, and the **order** is: per-file version check first, then
cross-file agreement. A disagreement is a corrupt case, not a version question.

**`kind` is checked too.** The JSONL header is `{"logmon_format":1,"kind":"logdata"}`,
and revision 1 ignored `kind` — so a case with its two data files swapped would load
cleanly with logs in the span store. An **absent** header (empty file) is positive
evidence of truncation, because the writer emits one even for zero records.

#### 3.3.1 Path safety — absent from revision 1 entirely

`yaml_str` round-trips `../../../../etc/passwd` perfectly; a string codec is not a path
validator, and revision 1 conflated them.

- The `.md` parameter **must be absolute**, mirroring `cases.create`'s own rule and its
  reason (`rpc_handler.rs:1908`): the broker runs as a service, so a relative path
  resolves against *its* working directory.
- `logdata.file` / `spandata.file` must equal `format!("{case}.logdata.jsonl")` /
  `.spandata.jsonl` — one string compare, since `case:` is front-matter line 2 — and
  resolve against the **document's own parent directory**.
- `domain` goes through `DomainId::new` (`domain.rs:39`), because it otherwise flows
  into `persist::path_for`'s `config_dir.join(...)` — a write outside the config dir.

Without these the daemon reads an arbitrary file and republishes its lines through
`logs.recent`, inverting D8's entire rationale.

#### 3.3.2 Bounds — absent from revision 1 entirely

`logdata.records` is a number in a text file. Revision 1 mapped it straight to buffer
capacity, bypassing the `MAX_DOMAIN_BUFFER_SIZE` gate (`rpc_handler.rs:410`) that
`domains.create` enforces *"rather than aborting the process on first ingest"*.
`records: 18446744073709551615` reaches `reserve_exact` and panics; `200000000` asks
for ~56 GB and aborts the process, taking every live domain with it.

**So: capacity is the count of records actually parsed, and front-matter `records` is a
cross-check that errors on mismatch — never an allocation size.** The count is bounded
by `MAX_DOMAIN_BUFFER_SIZE` before the file is opened, and there is a per-line ceiling.

#### 3.3.3 Record validation

The stores assume an ascending, distinct deque — `context_by_seq` does `position()`
then a slice, and `seq_set` is a `HashSet` so duplicates collapse there but not in
`entries`, desynchronising `len()` from `contains_seq`. So `from_records` **rejects**
non-ascending seqs, duplicates, seqs outside `[from, to]`, and `from > to`.

This also closes the hole where `verdict` — a hand-editable text field — becomes a
completeness guarantee: the epoch is seeded from what was actually loaded and validated,
not from a word in the document.

### 3.4 The seq axis — revision 1 was off by one in two opposite directions

Three consumers constrain the seeds, and revision 1 satisfied only the one its own test
checked.

| Consumer | Needs |
|---|---|
| `logs.export {}` — `to = current_seq()`, `from = max(origin+1, lost_below)` (`rpc_handler.rs:1251`) | `current_seq() >= to`, else `from > to` → window `None` → **`cannot_verify`** |
| `EpochLog::coverage(from, to)` — `covered = from >= first.seq_at_flip` (`epoch.rs:209`) | `origin <= from` |
| `spans.export` — `floor = max(lost_below, origin+1)` (`rpc_handler.rs:2365`) | `origin + 1 <= from`, else **every export reports `evicted_before_window: 1`** |

Revision 1 seeded the counter at `from` and the origin at `from`, which fails the first
and the third. The resolution:

```
seq counter  := seq_range.to        (not `from`)
epoch origin := seq_range.from - 1  (not `from`, and passed EXPLICITLY)
lost_below   := seq_range.from
```

`LogPipeline::new_with_seq_counter` hard-wires `EpochLog::new(seq_counter.current())`
(`pipeline.rs:98`), so `from_records` must take `epoch_origin` as a parameter rather
than inherit that line. The two seeds are then independent, which is what makes both
constraints satisfiable at once.

**Seeding the counter at `to` also removes the collision hazard** that revision 1's
unsound "structural sealing" argument was papering over: if anything ever did call
`SpanStore::insert`, it would take `to + 1` — above every loaded record — instead of
`from + 1`, which belonged to one.

**The regression test must use the DEFAULT call shape** (`logs.export {}`), not an
explicit range. Revision 1's T14 used an explicit range, which is the one shape where
both errors cancel.

### 3.4.1 D2 — the registry is a rendering, not data

**Three lenses converged here, and it changes what D2 costs.** The 14-key front matter
has no `registry`. The registry appears once, in the document *body*, as a markdown
table (`document.rs:885`) that is lossy three ways:

- **values** pass through `cell()` → `flatten()` (`render/escape.rs:36`), which collapses
  `\n \r U+85 U+2028 U+2029` to spaces and trims. Not invertible.
- **timestamps** render as `approx_age` — largest whole unit, `"3m"`, `"1w"`
  (`document.rs:219`). A fact validated 13 days before capture reads `1w`; restoring
  `validated_at = captured_at - 1w` flips a 7-day-TTL fact from expired to within.
- **rows past `REGISTRY_RENDER_CAP` (64 KiB) are dropped**, and the writer's own recovery
  advice is to read them from the live domain — the one thing the postmortem scenario
  guarantees is gone.

So D2 as stated is **not implementable from the current format**. Three ways out:

| | Cost |
|---|---|
| **(a) add a machine-readable registry block to the document** | additive format change; `FORMAT_VERSION` stays 1, matching the collector path's documented additive discipline (`collector/persist.rs:184`). D2 becomes exact, TTLs and all |
| (b) restore the rendered facts, labelled as a rendering | no format change; silently drops everything past 64 KiB and rounds every timestamp. Restoring a truncated registry as if whole is the class of silent-wrong this design exists to fight |
| (c) drop D2 | contradicts a direct instruction |

**(a) was chosen — user, 2026-08-04 — and is BUILT.** A fenced
` ```json logmon-registry ` block in §5 carries `RegistryFact` verbatim; one struct
serves the table and the block, so the two halves cannot drift. `FORMAT_VERSION` does
not move, matching `collector/persist.rs:184`.

Two consequences worth carrying into the loader:

- **`Ok(None)` is a real answer.** A case written before the block existed still
  parses, and the loader must report that provenance could not be restored rather
  than reconstructing it from the table and presenting the rounding as exact.
  Malformed is a *different* answer and errors, or a truncated document would restore
  an empty registry and call it complete.
- **`dropped` is machine-readable and may be non-zero.** `REGISTRY_RENDER_CAP` now
  governs the whole of §5 rather than the table alone — the first draft charged only
  the table and §5 ran to 145 KiB against a 64 KiB cap. Both halves therefore carry
  exactly the same facts, and when `dropped > 0` the loader says the restored
  provenance is a **subset**, never presents it as the registry.

### 3.5 What the loaded domain carries

| From front matter | Becomes |
|---|---|
| parsed record counts (cross-checked against `logdata.records` / `spandata.records`) | `log_buffer_size`, `span_buffer_size`, bounded by `MAX_DOMAIN_BUFFER_SIZE` |
| `seq_range.from` | `lost_below` on both stores |
| `seq_range.from - 1` | the epoch origin, passed explicitly |
| `seq_range.to` | the seq counter's seed |
| `captured_at` | the domain's frozen clock (§3.7) |
| `incarnation` | `set_reserved("/logmon/incarnation", …)` on the **in-memory** registry (§3.6) |
| `anchor.value`, `anchor.seq` | a bookmark (§3.8) — note the emitted key is `value`, not `label` |
| `verdict`, `clamped`, `requested_*_missing` | surfaced by `get_status` (§3.9) |

**`total_received` / `total_stored` / `malformed_count` must not be defaulted.** Setting
received = stored = N asserts *"this domain received N and stored all N, none
malformed"* about a 5,000-record window cut out of a stream that dropped and evicted.
Nothing in the case carries the true values, so `get_status` **suppresses** these three
on a postmortem domain rather than letting a default answer them.

### 3.6 The registry must be in-memory only

`domain_data` is keyed by **name** and persisted, and its lifetime is documented as
*"by file, not by domain … a deleted domain that is re-created adopts its own history"*
(`domain_data/map.rs:86`). Merely resolving it calls `stamp_boot(..., Utc::now())`
(`map.rs:61`), writing the **reader's** broker version and today's `first_seen`.

So a naive D2/D3 would: write a three-month-old case's provenance into a file shared
with the live domain of that name; leave it there after `domains.delete`, which does not
`forget()` or remove the file; and let a later live domain of the same name adopt it.

**Therefore the postmortem domain's registry never goes through
`DomainDataStore::for_domain`.** It is an in-memory `Registry` carried on
`PostmortemInfo`, and `resolve_domain_data` checks for it first. This also removes
`stamp_boot` from the postmortem path entirely, so D3's overwrite has nothing to race.

### 3.7 The frozen clock — two of revision 1's three sites were wrong

**The rule, stated so it cannot be got backwards:** the frozen clock governs *facts
about the captured system*. It does **not** govern *the reader's own actions*.

Revision 1 then violated its own rule. Corrected:

| Site | What it is | Clock |
|---|---|---|
| `rpc_handler.rs:773` (`domain_data.get`) | `age`, `is_expired`, the `validated_before_secs` cutoff — three uses, not one | **frozen** |
| `rpc_handler.rs:647`, `:708` | the `now` that becomes `created_at`/`validated_at` of a **newly written** fact | **real** — revision 1 froze these, dating today's annotation at capture time |
| the loader's own restore | each fact's own `validated_at` from the case | **neither** — a third clock |
| `store/bookmarks.rs:182` | the reader placed it today | **real** |

**And `domain_to_info` is INERT, not wrong.** It derives `idle_secs`/`stale` from
`d.metrics.liveness()` (`rpc_handler.rs:35`), whose fields are `None` for every slot
never written (`receiver/metrics.rs:188`). A loaded domain has no receivers, so
`idle_secs = None` and `stale = false` **whatever clock is consulted**. Freezing it
changes nothing; revision 1 measured the live path.

That is a stronger falsehood than a stale reading — the domain reports *not stale*. The
honest fix is §3.9, not the clock. `record_received` is module-private
(`metrics.rs:173`), so seeding liveness from `captured_at` would need a new
`ReceiverMetrics` constructor; **deferred, and named as deferred.**

`captured_at` in the future — clock skew between a production box and a dev laptop is
this design's own scenario — needs `.max(0)`, which `document.rs:230` already learned
and `document.rs:384` did not.

### 3.8 Landing on the anchor

Materialise `anchor.value` + `anchor.seq` as a bookmark. **Still a proposal, not a
decision.** Two constraints the gate found: `is_valid_bookmark_name` allows only
`[A-Za-z0-9_-]{1,64}` (`bookmarks.rs:109`), and a bookmark-kind anchor's spelling may
be `other-session/name`, which is rejected — loudly, so it is a validation branch rather
than a hazard. And `bookmarks.add` defaults `start_seq = current_seq()`
(`rpc_handler.rs:2453`); with the counter at `to` that is the top of the window, which
is the live-domain meaning.

### 3.9 `get_status`

Revision 1 named `domain_to_info` as the seam. **`handle_status` never calls it**
(`rpc_handler.rs:1340` builds a hand-rolled `json!`); `domain_to_info` feeds only
`domains.create` and `domains.list`.

So the postmortem fields go on `StatusGetResult`, which is **pinned**:
`capability_skew.rs:120` asserts the daemon sends no field that struct does not carry.
The change is protocol struct + `capability_skew` update, and it must announce:
postmortem, `captured_at` with real elapsed time as an absolute, the capture's
`verdict`, and the suppression of the three counters §3.5 names.

### 3.10 Dispatch

`load_case` creates and inserts a domain, so it must take `create_lock` — a
`tokio::sync::Mutex` (`rpc_handler.rs:128`) — for the same reason `domains.create` does:
two concurrent loads on one name otherwise both pass the existence check and
`DomainRegistry::insert` is documented *"Insert (or replace)"* (`domain.rs:258`), so the
second silently replaces the first.

`handle()` is synchronous and cannot `.lock().await`, so **`load_case` is an arm in
`handle_async`** beside `domains.create`. It counts toward `max_domains`.

---

## 3.11 The bundle (user-directed 2026-08-04)

Three files sharing a stem desync or lose a sibling in transit, and the design's
own scenario is moving them between machines. So a case becomes **one artifact**.

**This reverses a recorded decision, and the reason it may be reversed is the
reason it was recorded.** `cases/mod.rs` says the archive is uncompressed because
*"indexing belongs to whatever walks the directory, so putting a codec between
the archive and `grep` costs more than the bytes are worth."* `grep` is no longer
the consumer: `load_case` reads a case and MongoDB manages the collection. The
premise expired, so the decision is re-derived rather than inherited — which is
what writing the reason down was for.

Measured on real fixture bytes, 5000-record capture, two models:

| Model | Ratio |
|---|---|
| every message unique (floor) | **18.9×** — 1.9 MB → 100 KB |
| repetitive service traffic | **69.5×** — 1.7 MB → 24 KB |

Even the floor makes a full capture emailable, which is a stated transport.

### Layout

```
<stem>.case.zip
├── <stem>.md               document — the human half; the §5 table stays
├── <stem>.logdata.jsonl    evidence
├── <stem>.spandata.jsonl   evidence
└── <stem>.registry.json    provenance, machine-readable, COMPLETE
```

Stem-prefixed entries, so `unzip` reproduces exactly the loose layout: the bundle
is a container, not a new format. `load_case` accepts a `.case.zip` **or** the
`.md` of an unzipped case, which it needs anyway for a case someone is working on.

**The `zip` dependency sits AROUND the contract, not on it** — the asymmetry with
the YAML crate §3.2 refused. If the crate were abandoned, every archive is still
readable by every unzip in existence; a YAML parser would have been load-bearing
on the format itself.

### Moving the registry out of the `.md` (user-directed)

Two consequences beyond tidiness:

- **`REGISTRY_RENDER_CAP` stops applying to it.** That cap bounds the *document*.
  As its own compressed entry the registry costs the document nothing, so
  `registry.json` carries **every** fact: `dropped` is always 0 and the loader
  restores the complete registry. §3.4.1's "restored provenance is a subset"
  caveat disappears.
- **It fixes a gate finding.** The table's truncation note currently advises
  *"read them with `get_domain_data` on domain X"* — which presumes the live
  domain still exists, the one thing the postmortem scenario guarantees it does
  not. It now points at a sibling entry that travels inside the bundle.

### Options — logmon does not know the intended usage

The caller knows whether this is going by email, into a document store, or staying
local. So `cases.create` takes:

| Param | Default | Effect |
|---|---|---|
| `separate` | `false` | loose files instead of a bundle |
| `uncompressed` | `false` | stored entries rather than deflated |
| `omit_logdata` | `false` | document + provenance only, no bulk log evidence |
| `omit_spandata` | `false` | same for spans |

Defaults produce the complete, single, compressed artifact.

**`uncompressed` with `separate` is inert and therefore errors**, per the rule
already governing a sealed domain: an accepted no-op reads as a thing that
happened.

### OMITTED IS NOT EMPTY — the load-bearing rule of this section

`spandata: {records: 0}` means **"we captured none"**, and the writer emits a
38-byte header-only file precisely to say so — `write.rs`'s own test comment is
*"Absent cannot be told from 'we captured none'."*

If omission produced that same front matter, a reader could not tell **"this
system had no traces"** (a finding about the system) from **"someone chose not to
ship them"** (a fact about the capture). A loaded case would answer *"what was
the slowest span three months ago?"* with a silence that looks like evidence.

**The encoding is the file's presence** (user-directed 2026-08-04), not a flag —
and it is available precisely because the writer already emits a header-only file
for a zero-record capture. Absence was previously impossible, so it is free to
carry meaning now:

| Front matter | Entry | Means |
|---|---|---|
| declares `spandata` | present | evidence; `records: 0` is **"we captured none"** |
| no `spandata` key | absent | **omitted at capture** |
| declares `spandata` | **absent** | corrupt or desynced — an error, never a guess |

Better than a flag because the front-matter key's presence mirrors the file's, so
the two cannot contradict each other by construction; a flag can disagree with
reality. It makes `logdata`/`spandata` **optional** keys in the parser, and the
third row is why "optional" must not collapse into "ignore if missing".

Consequently:

- the document's §2 — "what this capture can and cannot show" — states the
  omission in prose, beside the eviction and filter facts it already leads with.
  A human reading the `.md` must not have to infer it from a line that is not
  there;
- **`load_case` refuses span queries on a domain whose spans were omitted**,
  naming the omission, rather than serving an empty span store. Same for logs.
  An empty result is an answer; this is the absence of one.

Omission is a choice, not a capture limitation, so it does **not** fold into
`verdict`, which grades what the capture could vouch for.

---

## 4. The writer fix (D7)

`before`/`after` count records in the **shared seq space**, so one interval still
describes both files.

**Not via `SpanStore::context_by_seq`, which revision 1 nominated.** That locates the
anchor with `position(|s| s.seq == seq)` — an exact match in the span ring
(`span/store.rs:236`). The anchor is always a *log* seq and one counter feeds both
stores, so it returns `vec![]` for **every** case, always, after a full linear scan.
Building the fix on it would reproduce F1 inside the fix for F1.

**Use what `cases.create` already does for spans**: `SpanStore::newest_seq()` /
`oldest_seq()` (`span/store.rs:289`, `:308`) to find the span seqs bracketing the
window, then `for_each_matching` under a `lower_seq_range` filter
(`rpc_handler.rs:1987`).

Consequences: `w.to` becomes the newest record across both stores **within the
requested window** — not the global maximum, which would run every case to the top of
the ring — so `document.rs:531`'s sentence becomes true. One fix, two defects.

**Its own commit, before the loader**, with T12 red-first.

**User-visible:** a caller who passed `after: 50` used to get 50 log records and now
gets 50 records of either kind. `skill/logmon.md` and `crates/mcp/README.md` both
describe these parameters and must be updated — §7.

---

## 5. The refusal surface — there is no choke point

Revision 1 claimed "one check, not five". **False.** Dispatch is a flat `match` with
~50 arms and no shared pre-step (`rpc_handler.rs:174`). The only near-universal seam is
`resolve_domain` (`:166`) — **29 call sites, reads and writes alike**, so a check there
refuses `logs.recent` and kills the feature.

Worse, several mutators never resolve a domain at all and are invisible to any
`Domain`-keyed check: `filters.edit` (`:1465`), `collectors.edit` (`:2764`), `.remove`
(`:3208`), `.snapshot` (`:2877`), `.reset` (`:3184`), `.history` (`:2928`).

**So: a `require_live(&d)` call at each mutating site, plus a manifest-driven test that
every tool declared mutating in `mcp_tools::TOOLS` is covered.** That test — not a
choke point — is what catches the tool nobody has added yet. The claim being made is
weaker than revision 1's and it is the one that is true.

The real list, which revision 1 under-counted by six:

| Operation | Why |
|---|---|
| `add_collector`, `add_trigger`, `add_filter` | forward-facing; armed here they read as *"I measured and nothing happened"* |
| `collectors.edit/remove/snapshot/reset`, `filters.edit/remove`, `triggers.edit/remove` | same, and session-keyed — the check cannot be domain-based |
| `clear_logs` / `clear_domain` | the live buffer refills; this one cannot |
| `domain_data.update` / `.remove` | **edits or deletes the restored evidence** |
| `bookmarks.clear` | deletes §3.8's anchor bookmark with no way to recreate it |
| `create_case` | D6 |
| **`domains.create` on a postmortem name** | `ensure_idempotent` treats an unspecified port as matching (`:264`), and a postmortem domain has all ports 0 — so it currently **succeeds**, handing back a live-looking domain that silently swallows everything sent to it |

`domains.delete` must stay allowed — and the loaded domain's `DomainSource` must not be
`Config`, which `:453` refuses, or §5's own escape hatch does not work.

Bookmarks (except `clear`) and cursors stay.

---

## 6. Test table

| # | Property | Seam | Level — and can the failure be seen there? |
|---|---|---|---|
| T1 | Escapes round-trip; strictness refuses what the emitter cannot produce; a `}` in a quoted value does not close the mapping | `cases::read` | unit. **Built and green** |
| T2 | A real fixture case loads | `cases::read::load` | integration |
| T3 | Seqs are preserved | store contents | integration — a unit test of `from_records` cannot see that the loader passed the right records |
| T4 | Newer format refuses; older accepted; version checked first | version check | unit. **Built and green** |
| T5 | Three-way disagreement, wrong `kind`, absent header | version + kind check | unit |
| T6 | **`logs.export {}` on a `complete` case reports `complete`** | epoch seeds | integration. **DEFAULT call shape — the explicit-range shape is where both revision-1 errors cancel** |
| T7 | `spans.export` over the window is not `evicted_before_window` | epoch origin | integration — the second half of §3.4 |
| T8 | Reader actions use the real clock | `BookmarkStore` | integration — the §3.7 inversion |
| T9 | Loading never touches the on-disk domain_data of that name | filesystem | integration — **negative control: assert no file appears** |
| T10 | Every mutating tool in `TOOLS` is refused | the manifest-driven test | integration — this is §5's real mechanism |
| T11 | Path traversal refused; non-absolute refused; `records` mismatch refused; oversize refused | validation | unit, one per §3.3.1/§3.3.2 |
| T12 | **Writer: window covers both stores** | `cases.create` | integration — **must go RED under current code** |
| T13 | Writer: `w.to` is the newest record **within the requested window**, not the global max | window derivation | unit |

**T12 and T6 are mutation-verified by the author at the moment of writing.**

**Deliberately untested:** `HashMap` ordering across a round trip. It is not stable and
no test should assert it is.

---

## 7. Docs surface

Walked from `docs/process/docs-surface.md`. README (tool count = `TOOLS.len()`),
`skill/logmon.md` (**including `before`/`after` semantics**, §4), CHANGELOG,
`docs/medium-article.md`, `crates/mcp/README.md` (**same**), `crates/sdk/README.md`.
Add `crates/protocol/src/mcp_tools.rs` to that list — the gate noted it is a surface the
tool-count rule already implies.

---

## 8. Deliberately not done

- **Replay** — proposal §8.
- **Unattended case writing** — proposal §6.
- **Cross-domain diff.** The stated payoff is *"compare it to the current value"*, and
  loading gives two domains and two calls to eyeball. A known gap, not a discovery.
- **Seeding `ReceiverMetrics` liveness from `captured_at`** — needs a new constructor;
  §3.9 carries the truth instead.
- **F2** (trace anchoring), **F4** (the CLI's array-of-object gap — file it).

---

## 9. Status

Built and green: §3.2 (the reader), the fixtures. Everything else is designed and
unbuilt.

---

## 10. Where the gate's findings landed

Four lenses, ~615k subagent tokens. **Convergence was high and that is the signal**: six
findings were reached independently by two or three lenses, and every one was real.

| Finding | Lenses | Landed |
|---|---|---|
| The preserved-seq seam EXISTS and is public; "structural sealing" is false | grounding | §3.1 — rewritten; the constructor argument now rests on fidelity |
| Seq counter at `from` breaks `logs.export {}` | implementability, grounding | §3.4 — counter seeds at `to` |
| Epoch origin at `from` makes every `spans.export` report truncation | implementability | §3.4 — origin is `from - 1`, passed explicitly |
| D2's registry is a lossy rendering, not data | **all three code lenses** | §3.4.1 — needs a format addition and sign-off |
| §5's single choke point does not exist | implementability, failure | §5 — rewritten as per-site checks + a manifest-driven test |
| `domain_to_info` is inert on a receiverless domain | implementability, failure | §3.7 |
| §3.5 froze two *write* timestamps — the inversion the rule forbids | implementability, grounding | §3.7 |
| §4's named seam always returns empty | implementability, failure, grounding | §4 — `newest_seq`/`for_each_matching` |
| Registry contamination: name-keyed file outlives the domain | implementability, failure, grounding | §3.6 — in-memory registry |
| Path traversal via front-matter file pointers | failure | §3.3.1 |
| `records` → capacity is an unbounded allocation | failure | §3.3.2 |
| `domains.create` on a postmortem name silently succeeds | failure | §5 |
| `kind` unchecked; absent header unhandled | failure | §3.3 |
| TOCTOU on the name; `create_lock` is async-only | implementability, failure | §3.10 |
| `malformed_count` / `total_*` assert zero loss | implementability | §3.5 |
| T14 used the one call shape where both errors cancel | implementability, grounding | T6 |
| Cold reader: 9 questions closable by pasting a real front matter | cold reader | fixtures committed; §3.2 points at them |
| ~10 citation drifts | grounding | corrected in place |

**The honest reading: this design pass failed and the gate caught it.** Revision 1's
central argument was built on a premise nobody checked — the same failure mode duty 0
exists to prevent, committed one layer down, in a claim about what *does not* exist.
Claims of absence are the ones to check hardest; two minutes with `grep "fn append"`
would have found it.
