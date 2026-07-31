# Domain data and case documents — v1

**Status:** draft for design gate. No code written.
**Tier:** T2 — mints two persisted contracts (the `domain_data` file format and the case
document front-matter) that outlive the code.
**Seams verified at:** `53ebd34`. Every row in §1 names a **constructor and a caller**, not
a type definition — see §1.1 for why that distinction is stated.
**Supersedes** the first draft of this file (three lenses, ~30 findings). §9 records what
was cut and why.

---

## 0. What this is for

Elusive bugs are the ones you cannot reproduce on demand. The evidence exists for seconds
in a ring buffer and is then gone, and by the time anyone knows it mattered it is gone.
logmon can already *see* everything needed and keeps none of it.

A **case document** is a durable, self-contained artifact capturing what was happening
around a moment. Provenance is what makes it evidence rather than a log dump, so
**`domain_data`** comes first: a small key/value registry per domain, copied into every
document.

**The failure this is built against:** *wrong information is worse than none.* Every
choice below follows from that — including the scope cuts in §9, several of which exist
because a mechanism could not tell a reader the truth about its own completeness.

---

## 1. Seams — verified at `53ebd34`

| Seam | Evidence |
|---|---|
| Per-mutation collector persistence, and its one hard constraint | `Registry::write` (`collector/registry.rs:399`) → `persist::save` (`collector/persist.rs:401`) → `atomic_write`. Its doc comment (`registry.rs:392-398`) is the constraint §3.5 inherits: *"`atomic_write` does two `fsync`s … a write under the lock lets one slow fsync stall span ingest daemon-wide until the 65 536-slot channel overflows and starts dropping. Bookkeeping must not be able to cost telemetry."* Write failure is logged and named — *"it will not survive a restart"* — never silent. |
| **Documents are written by the CLIENT, not the daemon** | `std::fs::write` at `mcp/src/cli/collectors.rs:671,686` and `mcp/src/server.rs:530`. `collectors.document` returns `content` + `sidecar_content` over RPC and writes nothing. §5 keeps it that way. |
| An entry reaches the store **conditionally** | `daemon/log_processor.rs:167-185`: stored only if a post-window is open, or the domain's session filters match. One `add_filter` anywhere in the domain makes storage matches-only. §4.2 exists because of this. |
| `logs.export` has no seq range | `LogsExport { count, filter }` (`protocol/src/methods.rs:282`). Params are read by walking raw JSON (`rpc_handler.rs:692-704`), so §6's additions are absent-safe for existing callers. |
| Sidecar naming collides in a flat archive | `format!("{}.sidecar.json", safe_name(…))` (`document.rs:1690`); `safe_name` (`:1874`) collapses non-alphanumerics to `_`. §5.3 names files itself. |
| Material-first section order | `document.rs:839,1068,1156,1349` — `1. What moved` → `2. What to do next` → detail → definitions. |
| Recorded artifacts carry their own definition | `StoredSnapshot.def` (`collector/history.rs:84`), pinned by `a_snapshot_records_the_definition_it_was_taken_under`. |

### 1.1 What the first draft got wrong here, and why the table format changed

The first draft recorded *"three-way domain lifetime — **Confirmed**"* on the strength of
`DomainSource::{Config, Persistent, Ephemeral}` existing and `Persistent`'s doc comment
describing durable re-creation. All of it was aspirational text beside dead code:

- `domains.create` **rejects** `persist=true` (`rpc_handler.rs:277-282`).
- Nothing constructs `DomainSource::Persistent` — zero constructors.
- Config domains persist their *declaration*; `persistence.rs:89` says plainly that
  **"data is never persisted."**
- `persist=true` was designed, gated and **deferred** in
  `2026-07-14-domains-and-broker-improvements-design.md:752`, pending *"a durable-domain
  consumer."*

`domain_data` is that consumer. It does not inherit a persistence seam — **it must bring
one** (§3.5). The rows above therefore cite a constructor and a caller rather than a type,
because a doc comment describes intent and is not evidence of a code path.

---

## 2. Scope

**In:** the `domain_data` registry with its own persistence; `cases.create` (manual);
a seq range on `logs.export`.

**Out, deferred to v2:** **watches** — automatic case-writing on a filter match. §9.1
records why that is a design problem rather than an implementation one.

**Out permanently:** any query engine over the archive. The format is the contract;
indexing belongs to whatever walks the directory.

---

## 3. `domain_data`

### 3.1 Shape and path syntax

A flat map of path-shaped keys to string values, per domain:

```
/Versions/ht_server   1.0.2
/Action               full test suite
```

Flat, not a tree: prefix matching gives subtree operations, and flat keys serialise and
index trivially. Path rules, stated because §3.3 rejects "malformed" ones and §3.4 matches
prefixes:

- Begins with `/`; no empty segment (`//`, trailing `/` rejected), so each key has exactly
  one spelling.
- Segment characters: **ASCII** letters, digits, `_`, `-`, `.`. No spaces.
- Case-sensitive, compared byte-wise.
- Path ≤ 256 bytes; **value ≤ 4 KiB**; **≤ 256 keys per domain** — the registry is copied
  into every never-deleted document, so it is not a payload store.

**ASCII is a deliberate narrowing**, matching `session.rs:183`'s existing `is_valid_name`.
It rejects a team naming its own categories in a non-Latin script — a real cost, taken
because keys are correlation identifiers across an archive and mixed-script keys that look
identical are worse than a workaround. **Values are unrestricted UTF-8**, so borrowed
identifiers (image tags, semvers with build metadata, branch names) are unaffected.

**Values are escaped on emission** (`yaml_str`, `document.rs:1836`) — newlines, `---` and
control characters in a value would otherwise break the front-matter contract §2 depends
on. The first draft constrained keys carefully and left the other half of the same line
unguarded.

**Prefix matching is on segment boundaries.** `/Versions` matches `/Versions/ht_server`
and `/Versions`; it does **not** match `/VersionsOld/x`. Byte-prefix matching would let
`/Ver` delete `/Versions/*` — and `remove` has no undo.

### 3.2 Two timestamps

```
created_at    when THIS VALUE came into force
validated_at  when someone last confirmed it is still true
```

A single "last modified" conflates two questions. Set six days ago and never revisited is
a guess; the same value confirmed five minutes ago is evidence. Invariant:
**`validated_at >= created_at`**.

### 3.3 `domain_data.update(entries)`

Each entry is `{path, value?}`. **Value present** → set (differs ⇒ both timestamps move;
same ⇒ only `validated_at`). **Value absent** → validate only.

**Absent ≡ `null` ≡ validate.** Erasure is `remove`, never `set(null)`. Stated because a
client serialising a missing field as `null` would otherwise mean the opposite — the
absent-vs-zero trap that produced three defects in 0.8.0.

Per-entry outcomes: `created`, `updated`, `validated`, `unknown`, `rejected`.

**`unknown` must not silently create.** Key-only means "confirm what you have"; inventing
a valueless key is the helpful guess that becomes wrong data. It carries a **cause**
(`never_set` / `domain_has_no_registry`) rather than one label, because the remedy differs:
re-setting after a lost registry resets `created_at` and launders a months-old confirmed
fact into a fresh-looking one — §0's failure inverted.

One malformed entry does not reject the batch. Valid rows apply; rejected rows return
their reason.

### 3.4 The rest of the surface

| RPC | Tool |
|---|---|
| `domain_data.update` | `update_domain_data` |
| `domain_data.remove` | `remove_domain_data` — patterns; per-pattern match counts, and a pattern matching zero is **reported** |
| `domain_data.get` | `get_domain_data` — optional patterns, optional `validated_before` (duration or absolute); returns values with both timestamps |

**`stale` is a filter on `get`, not its own tool.** The first draft made it a tool, citing
a claim its own source does not make; the in-repo precedent is the opposite —
`2026-07-29-span-time-collector-design.md:1032`: *"a fourth reader on the same surface
would have been the weaker instrument."* The staleness sweep is instead surfaced where it
is acted on: §5.2's evidence line, and the skill (§8).

`remove` rejects patterns matching `/logmon/` — the first draft guarded `update` only.

### 3.5 Persistence — new work, not inherited

One JSON file per domain at **`<config_dir>/domain_data/<domain>.json`**, written through
on every mutation.

**The subdirectory is not cosmetic.** Domain names allow any alphanumeric
(`session.rs:183`), so `config` and `state` are legal — and a file written straight into
the config dir as `<domain>.json` would overwrite `config.json` or `state.json`. Both then
*parse successfully*, because every field of `DaemonConfig` and `DaemonState` has a
default: the broker comes back on default ports having silently lost every named session,
trigger, filter and bookmark, without even reaching the quarantine path. The collector
precedent already solved this and states why — `COLLECTORS_DIR` (`persist.rs:34-37`):
*"Keeps collector files out of the directory holding `state.json`, `daemon.pid` and the
socket, so the boot sweeps and this one cannot reach each other's files by accident."*
The first draft claimed to follow that precedent "exactly" and omitted the part carrying
the reason.

- Whole-file atomic write (temp + fsync + rename + directory fsync) — the mechanism at
  `persistence.rs:214-241`, which already has a boot sweep and a racing-reader test.
- **A per-domain write mutex, held across serialize-and-write and never across the data
  lock.** This is the load-bearing rule here. The collector precedent is safe with none
  because its files are keyed `(owner, name)` and have one logical writer; `domain_data` is
  keyed by **domain**, and many sessions bind one domain by design. Two concurrent updates
  can otherwise reach `atomic_write` out of order, and the older landing last silently
  drops the newer key from disk while memory keeps serving it — invisible until a restart.
  §0's failure produced by the persistence layer itself.
- Write failure is **logged and named** — "will not survive a restart" — never silent.
- Load quarantines unparseable files with the **numbered** scheme
  (`persistence.rs:268-283`), not `with_extension("json.corrupt")`, which overwrites the
  previous corrupt file.

**On the fsync-outside-the-lock rule:** it is worth keeping, but it is *not* load-bearing
here and the first draft was wrong to call it so. That rule matters for collectors because
`ingest_span` takes the *same* registry lock on every span (`registry.rs:905`); nothing on
any ingest path reads `domain_data`. Asserting it as central was the error §1.1 diagnoses —
a constraint taken from a doc comment without naming the caller that makes it true —
committed two sections later in this same document. The real hazard is the concurrent
write above. (One genuine inherited note, not a rule: RPC handlers are sync fns on tokio
workers, so an fsync blocks a worker — already true of the collector path.)

**Lifetime is by file, not by `DomainSource`.** The registry for domain `X` survives while
its file does; a domain re-created under the same name adopts it. This is chosen over
lifecycle-coupling because the alternative requires the deferred `persist=true` work
(§1.1), and because a registry that dies with an ephemeral domain is worthless to an
archive meant to be read years later.

**The consequence, stated rather than discovered:** a domain name is not an identity. Two
unrelated ephemeral `t3`s a week apart share a registry — and the skill actively
recommends the topology that produces this, one domain per test run.

**So the registry counts incarnations, not first sight.** Each time logmon creates a domain
whose file already exists, it increments `/logmon/incarnation` and stamps
`/logmon/incarnation_started`. A draft using `first_seen` alone could not work: it is
monotone, so after adoption it still reports era 1, and five unrelated eras are
indistinguishable from one long-lived domain — with the older timestamp reading as *more*
established rather than less.

This matters beyond provenance: a re-created domain restarts seq at 0
(`domain_lifecycle.rs:99`), so two archived documents for `t3` can carry overlapping seq
ranges meaning different records. **The document therefore records the incarnation beside
the seq range**, or its window identifier is ambiguous across eras.

### 3.6 The recommended key set

A registry whose keys are freely invented is a registry that cannot be correlated. Under
time pressure one person writes `/version`, another `/Versions/server`, and six months on
there is nothing to group by — which defeats the only reason the archive is global (§4.1).
So the set is **defined here, documented in the skill (§8), and reported against (§3.6.2)**.

It is a **convention, not a schema**: any path is still legal, nothing is rejected for
being absent, and a project with a concept logmon never anticipated just adds a key.

#### 3.6.1 The set

**Core — a document missing these cannot be acted on.**

| Key | Why it earns its place |
|---|---|
| `/Build/commit` | The only exact identity of the code. A version string is a label someone maintains; a SHA is what actually ran. |
| `/Build/profile` | `debug` or `release`. Load-bearing *for logmon specifically*: it is a timing instrument, and the two profiles differ by an order of magnitude. A case comparing durations across profiles is not a comparison. |
| `/Action` | What was being done, in prose — "full test suite", "checkout smoke, 20 iterations". Without it a reader has logs and no scenario. |

**Contextual — record when they apply; each answers a question a reader will ask.**

| Key | Answers |
|---|---|
| `/Versions/<component>` | "which release of *which part*" — plural because a system has several, and the failing one is rarely the one you upgraded |
| `/Build/branch` | "was this even mainline" |
| `/Env/host` | "only on CI?" — the first question about anything intermittent |
| `/Env/os` | platform-specific behaviour |
| `/Env/container` | image tag, when the runtime is not the host |
| `/Data/dataset` | which fixture or corpus |
| `/Data/seed` | **the highest-value key for the flaky-test case** (§0's motivating use): a recorded seed turns "fails 1 in 20" into a reproduction |

Rules: `<component>` and other leaf names are project-chosen and should be stable across
documents. Anything outside these namespaces is free for project-specific use;
`/logmon/` is reserved (§3.7).

#### 3.6.2 Coverage is reported, never enforced

A recommended set that only exists in documentation is the failure this project keeps
paying for — a capability nobody reaches for. So the document's evidence section reports
coverage:

> provenance: **2 of 3 core keys** — missing `/Build/commit`; 4 contextual keys present

That makes the convention self-enforcing at the moment it matters, without rejecting a
single legitimate key. It is the same rule as the rest of §4: absence stated as plainly as
staleness, because a reader cannot tell "not recorded" from "not applicable" unless the
document says which.

`update` never rejects a non-recommended key, and never warns. Coverage is a property of
the *document*, not of the registry.

### 3.7 The reserved `/logmon/` namespace

`/logmon/...` is written only by logmon; `update` and `remove` both reject agent access.
These keys are **stored**, not computed at document time, so `get_domain_data` can show
what logmon knows. Populated: `/logmon/domain`, `/logmon/first_seen` (when the *file* was
created — the signal that a registry is new, which is what §3.3's `unknown` cause leans
on), `/logmon/incarnation` and `/logmon/incarnation_started` (§3.5, which is what detects
*reuse*; `first_seen` cannot), and `/logmon/broker_version`.

`/logmon/broker_version` is **refreshed on boot**, not written once. Stored-and-stale would
put the pre-upgrade version into every document after an upgrade, as a fact, in the half of
the registry a reader has been told to trust.

`/logmon/*` keys do **not** count against §3.1's 256-key cap — otherwise logmon could lock
an agent out of its own registry.

Per-document facts (capture time, trigger) are **not** registry keys — they are document
front-matter (§5.2). The first draft conflated the two.

---

## 4. What a case document must be honest about

### 4.1 It records belief, not truth

`domain_data` is agent-authored and frozen into an artifact that is never deleted. A value
wrong at capture time is permanently wrong there. The document says so and carries both
timestamps.

**Staleness is reported as age, never as a verdict.** *"last validated 34 days ago"* is a
fact; *"provenance is current"* is a judgement logmon cannot make — `/Action` set three
minutes ago is already wrong if the action changed two minutes ago. Rendering age as
confidence would reproduce §0's failure inside the mitigation for it.

**An empty registry is stated as loudly as a stale one.** Absence must not read as
validation — the `matched: 0` / `zeroed_by` lesson, applied to provenance.

### 4.2 It distinguishes kinds of incompleteness — and `complete` is the hard one

The store is conditional (§1). `evicted_before_window` detects **eviction** and cannot
detect **never-stored**, so a window shaped by another session's filter would otherwise
report as complete. This is the document's most important correctness property: "nothing
appeared before the error" must not read as absence of cause when it is absence of
recording.

Verdicts, pessimistic when a range is mixed:

| | |
|---|---|
| `complete` | the range lies wholly inside an unfiltered epoch **and** no eviction was detected **and** nothing was capped |
| `evicted` | the ring dropped part of the range |
| `filtered` | a session filter was narrowing the store for some of the range — **which filters, and over which seqs** |
| `partial` | the range straddles a trigger post-window, during which entries were stored *unconditionally* while the rest was filtered |

**A point-in-time filter read cannot compute this.** The first draft said "record the
domain's active filter set at capture" — but that is a snapshot of a time-extended
property, and three routine events empty it silently: an anonymous session is **removed**
on disconnect and takes its filters with it (`session.rs:271-279`), `remove_filter`, and
`set_domain` re-binding a session out of the domain. Session A filters, the run happens,
A disconnects; agent B captures and sees no filters, so the document asserts "no filter
was narrowing the store" over a window that recorded a tenth of it.

**So the daemon maintains a per-domain narrowing marker**, updated by the mutations that
can flip *does this domain have any filter* — `filters.add/remove/edit`, `set_domain`,
session disposal — stamping `pipeline.current_seq()` and the filter strings at each flip.
`complete` is then claimable only for a range wholly inside an unfiltered epoch. This is
new recording, and more of it than one field.

**Two further sources of false `complete`, both closed at the source:**

- **A capped export satisfies "asked N, got N".** `recent_with_scanned` walks newest→oldest
  (`memory.rs:58-101`), so `count` drops the **oldest** entries — the context *before* the
  anchor, which is the half that matters. §6 therefore caps outward from the anchor, and a
  capped result is never `complete`.
- **Seq is shared with spans.** One `SeqCounter` feeds both the pipeline and the span store
  (`domain.rs:176`), so a 200-seq range holding 160 spans returns 40 logs with nothing
  missing. Completeness is therefore defined over **stored-entry provenance**, never over
  seq arithmetic or counts.

**An empty store is "cannot verify", not "no eviction".** `evicted_before_window` returns
`None` when `buffer_oldest_seq` is `None`, so a capture after `clear_domain` would
otherwise read as *0 entries, complete*.

---

## 5. `cases.create` — the manual path

### 5.1 Shape

`cases.create(reason, anchor, prefix?, before?, after?)` → the document and its sidecar,
returned **as content**, written by the client (§1). No daemon-side archival writer in v1:
the existing path already writes documents, and adding one would bring file naming,
atomicity, ENOSPC handling and concurrency — all of which belong with watches (§9.1), the
only feature that genuinely cannot use a client.

**`anchor` is explicit** — a seq, a bookmark name, or a `trace_id`. `before`/`after` are
relative to it, and §5.2 makes the anchor's message the headline and a front-matter key. An
implicit "newest entry" anchor would headline a case created five minutes after the fact
with whatever happened to arrive last, which fails §5.2's own test that a headline must
distinguish two documents. The first draft's signature had the windows and not the point
they were measured from.

`reason` is **required**. A manual case that cannot say why it exists has no provenance,
and unlike a watch there is no filter standing in for one.

### 5.2 Structure: discriminating first, then qualifying, then bulk

Front-matter (the index surface: registry copy, capture time, trigger, anchor message),
then:

1. **Headline** — the anchor entry's own message, the time, the domain. It must identify
   *this* incident: a headline that cannot distinguish two documents is not an index.
2. **Evidence** — §4.2's verdict, and provenance age. Before anything it qualifies; a
   caveat reached after 400 lines has already failed.
3. **What to do next** — the capturer's guess. `document.rs`'s house order has this
   second, and it is the most valuable thing in the file six months on.
4. **Anchor entry and neighbours.**
5. **Provenance** — the registry copy, each key with its age.
6. **Collector state** (§5.4).
7. Sidecar pointer.

**Sidecar** carries the full window and span trees, following `document.rs`'s rule: bulk
*moves*, with a pointer, rather than being cut without saying so.

### 5.3 Naming

`<prefix>-<UTC timestamp>-<short hash>.md`, sidecar `.sidecar.json` beside it. The hash is
required: `safe_name` collapses punctuation (§1), so prefix+timestamp alone collides in a
flat archive.

### 5.4 Collector state at capture

Include each collector's current numbers and any snapshot it holds. Two constraints from
§1's lock rule:

- Projection is computed **outside the registry lock** — it sorts every retained duration,
  which under the lock would stall ingest.
- **Which collectors** are visible must be decided, not assumed. `Registry::list` filters
  by **owner only** (`registry.rs:617`) and never by domain, while `ArmedCollector.domain`
  is a pin that a later `use_domain` does not move. So "the calling session's collectors"
  is wrong twice: it can embed collectors measuring a *different* domain, and it misses
  collectors armed on *this* domain by anyone else — the CLI connects as session `cli`
  while the shim uses its own, so an MCP-created case would see none of the CLI's.
  v1 therefore selects **by the case's domain, across owners**, and names the owner beside
  each collector.

If no collectors are armed **on this domain**, the section says so in those words.
Omission reads as "nothing interesting", and the first draft's session-scoped wording would
have printed a claim about the domain that was false.

---

## 6. Seq range on `logs.export`

Add `from_seq` / `to_seq`. Absent-safe (§1).

**They are lowered into `SeqFilter` qualifiers before resolution, not carried as loose
params.** `evicted_before_window` reads its lower bound out of the *parsed filter* via
`resolved_lower_bound` (`filter/parser.rs:745-773`), which matches `Qualifier::SeqFilter` —
today produced only by `b>=` / `c>=` resolution. A top-level `from_seq` would never reach
it, so a range whose start had already rolled out of the ring would come back
`truncated: false` and §4.2 would read that as `complete`. Lowering makes them compose with
a bookmark bound through the existing max-of-lower-bounds rule and feeds the detector for
free. State the inclusivity: `SeqFilter::Gt` is strict.

`count` caps **outward from the anchor**, not from the newest end (§4.2), and a capped
result is never `complete`. Cursor qualifiers (`c>=`) are **refused** in a capture: they
commit the cursor (`rpc_handler.rs:706-717`), so gathering evidence would advance the
caller's read position. There is precedent for refusing them at `rpc_handler.rs:606`.

---

## 7. Hazards

| # | Hazard | Handling |
|---|---|---|
| H1 | Registry rots; documents inherit confident-shaped stale provenance | Two timestamps; age never rendered as a verdict (§4.1); `validated_before` on `get` |
| H2 | A window silently shaped by another session's filter reads as complete | §4.2's three-way verdict — the correctness property, not a nicety |
| H3 | A lost registry makes re-setting launder old facts as new | `unknown` carries a cause (§3.3); `/logmon/first_seen` (§3.5) |
| H4 | An fsync under a lock stalls ingest | §3.5's no-lock rule, inherited verbatim from the collector precedent |
| H5 | Archive grows unbounded | v1 is manual-only, so growth is caller-paced. `status.get` reports count and bytes; a WARN crosses a threshold. Real bounding belongs with watches (§9.1) |
| H6 | A domain name is reused and two eras share a registry, with overlapping seq ranges | `/logmon/incarnation` counts eras and the document records it beside the seq range (§3.5). A monotone `first_seen` cannot do this and was the draft's error |
| H7 | A case captures a window whose evidence was already evicted | Inherent to a manual path. §4.2 makes it visible, which is the honest answer and part of why watches matter (§9.1) |

---

## 8. Skill and discoverability

Nine tools were proposed; v1 adds **four**. The Store report's finding was that
documentation, not capability, was the binding constraint, and this project has since paid
for that twice — so `skill/logmon.md` changes **in the same commit as the code**, not
after. Specifically:

**1. The key set from §3.6, as a table an agent can act on**, with the core three called
out and the seed key argued rather than listed — `/Data/seed` is what turns "fails 1 in 20"
into a reproduction, and it is the key most likely to be skipped because recording it feels
like bookkeeping at the moment it costs nothing and pays later.

**2. When to set them.** The set is worthless if it is populated once and rots (§4.1), so
the skill names the two moments rather than leaving it to judgement:

- **At session start**, one `update_domain_data` restating everything currently believed —
  versions read from a lockfile, the commit from `git rev-parse`, the profile. The
  per-entry outcomes make this cheap and informative: what came back `created` is news,
  `validated` is confirmation, and `unknown` means the registry was lost under you (§3.3).
- **When the answer changes** — a deploy, a branch switch, a different scenario. `/Action`
  in particular is stale within minutes of switching tasks, and a stale `/Action` is worse
  than an absent one because it reads as fact.

**3. When to reach for `create_case`**, in the bold "reach for X when…" form the
`profile_traces` fix used — a comparison table alone is what left `profile_traces` unused
in the one production trial we have.

**4. How to read the evidence verdict**, including that `filtered` and `partial` are not
warnings to skim past: they say the window is not what it appears to be, and a conclusion
drawn from "nothing appeared before the error" is unsound under either.

A capability nobody reaches for is the failure this project keeps repeating — and the
recommended key set is the part most likely to repeat it, because a convention is exactly
the kind of thing that lives only in prose. §3.6.2's coverage line is the mechanism that
stops that; the skill is what makes the first document have anything to report.

---

## 9. What was cut, and why

### 9.1 Watches — deferred, as a design problem

Three lenses converged on watches from different directions, which is why this is a
deferral rather than a fix list:

- **No on-ramp.** At the moment a user would arm one, they do not know the filter — that
  is what they are hunting. The only filter available pre-failure is `l>=ERROR`, already
  armed by default. The gesture reads as redundant and gets skipped.
- **Cannot reach the machinery.** `max_pre_window_for_domain` (`session.rs:962`) iterates
  **sessions only**, so on an unattended domain the pre-buffer capacity is **0** — the
  watch's entire purpose. The storage post-window is keyed on `SessionId`.
- **The flagship value is empty in the flagship scenario.** Anonymous sessions' collectors
  are cleared on disconnect (`server.rs:1062`), so a watch firing overnight finds none.

v2 must design the **trigger → case bridge** rather than a parallel mechanism: a trigger
already fires, already flushes the pre-buffer, already opens the storage window. That is
the on-ramp, and it is a smaller feature than what was proposed.

### 9.2 `cases.find` — cut

Undefined query language, competing with `grep`, on an archive §2 says needs no index.
Cutting it makes the anchor message in front-matter (§5.2) mandatory rather than optional.

### 9.3 `stale` as its own tool — cut

§3.4. The citation was wrong and the in-repo precedent says the opposite.

---

## 10. Test list

**Verification:** both timestamps round-trip through a restart; same-value update moves
only `validated_at`; key-only validates; `unknown` carries a cause; remove-by-prefix
reports per-pattern counts; the registry survives a daemon restart and a domain re-created
under the same name; a document embeds the registry and the anchor message.

**Adversarial:** `{path}` and `{path, value: null}` behave identically; key-only on a
missing key does **not** create; one malformed entry does not reject the batch; `update`
**and** `remove` both reject `/logmon/`; `validated_at >= created_at` after every
operation; a value containing a newline and `---` round-trips through front-matter without
breaking it; **a capture taken while another session holds a filter reports `filtered`,
names the filter, and does not report `complete`**; an empty registry is stated, not
omitted; no collectors armed produces a section saying so; a persist failure is logged and
the call still returns.

**The recommended key set (§3.6):** coverage names the **missing** core keys, not only a
count — "missing `/Build/commit`" is actionable where "2 of 3" is not; full coverage still
prints the line rather than omitting it, since silence is indistinguishable from a document
that never checked; **zero recommended keys reports `0 of 3`** rather than omitting the
section, that being the case most likely to render as nothing at all. And the
false-positive guard, because a convention hardening into a schema is how this goes wrong:
`update` **accepts a non-recommended key with no warning, no rejection, and no effect on
any other entry's outcome** — coverage is a property of the document, never of the
registry.

---

## 11. Open questions for the gate

1. Per-key volatility hint (`/Action` staleness is minutes, `/Versions/*` is per-deploy).
   Deferred; the format must leave room, since retrofitting a field into a persisted
   contract is expensive.
2. Sidecar format — markdown for reading or JSONL for machines. The bulk is the half most
   likely to be machine-read.
3. ~~Whether `/logmon/first_seen` suffices for the reused-domain-name case~~ — **closed**:
   it cannot (monotone), so §3.5 counts incarnations instead.
