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

One JSON file per domain under the config dir, written **through on every mutation**,
following the collector precedent (§1) exactly:

- Whole-file atomic write (temp + rename), not append.
- **Called with no registry lock held.** This is the load-bearing rule, not a detail:
  `atomic_write` does two `fsync`s, and holding a lock across one can stall ingest
  daemon-wide (§1). `domain_data` mutations are agent-driven and rare, but the constraint
  is about the fsync, not the frequency.
- Write failure is **logged and named** — "will not survive a restart" — never silent.
- Load quarantines unparseable files rather than deleting them (`persist.rs`'s
  `LoadOutcome` precedent).

**Lifetime is by file, not by `DomainSource`.** The registry for domain `X` survives while
its file does; a domain re-created under the same name adopts it. This is chosen over
lifecycle-coupling because the alternative requires the deferred `persist=true` work
(§1.1), and because a registry that dies with an ephemeral domain is worthless to an
archive meant to be read years later.

**The consequence, stated rather than discovered:** a domain name is not an identity. Two
unrelated ephemeral `t3`s a week apart share a registry. `/logmon/first_seen` records when
the file was created so a reader can notice.

### 3.6 The reserved `/logmon/` namespace

`/logmon/...` is written only by logmon; `update` and `remove` both reject agent access.
These keys are **stored**, not computed at document time, so `get_domain_data` can show
what logmon knows. Populated: `/logmon/domain`, `/logmon/first_seen`,
`/logmon/broker_version`.

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

### 4.2 It distinguishes three kinds of incompleteness

The store is conditional (§1). `LogsExportResult.evicted_before_window` detects
**eviction** and cannot detect **never-stored** — so a window shaped by another session's
filter would otherwise report as complete. The evidence line therefore separates:

| | |
|---|---|
| `complete` | asked N, got N, no filter was narrowing the store |
| `evicted` | the ring dropped the older part — how much |
| `filtered` | a session filter was active, so non-matching entries were never stored — **which filters** |

The third requires recording the domain's active filter set at capture; it is not
surfacing an existing field. This is the single most important correctness property in the
document, because "nothing appeared before the error" otherwise reads as absence of cause
when it is absence of recording.

---

## 5. `cases.create` — the manual path

### 5.1 Shape

`cases.create(reason, prefix?, before?, after?)` → the document and its sidecar, returned
**as content**, written by the client (§1). No daemon-side archival writer in v1: the
existing path already writes documents, and adding one would bring file naming, atomicity,
ENOSPC handling and concurrency — all of which belong with watches (§9.1), which is the
only feature that genuinely cannot use a client.

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
- **Which collectors** are visible must be decided, not assumed: `Registry::list` filters
  by owner. v1 reads **the calling session's** collectors, which is the manual path's
  natural scope and needs no isolation change. Watches would need a different answer, and
  that is part of why they are deferred.

If no collectors are armed, the section **says so**. Omission reads as "nothing
interesting."

---

## 6. Seq range on `logs.export`

Add `from_seq` / `to_seq`. Absent-safe (§1). Interaction with `count` is defined: the range
bounds, `count` caps within it, and a bookmark-derived filter composes as an additional
predicate rather than being rejected.

---

## 7. Hazards

| # | Hazard | Handling |
|---|---|---|
| H1 | Registry rots; documents inherit confident-shaped stale provenance | Two timestamps; age never rendered as a verdict (§4.1); `validated_before` on `get` |
| H2 | A window silently shaped by another session's filter reads as complete | §4.2's three-way verdict — the correctness property, not a nicety |
| H3 | A lost registry makes re-setting launder old facts as new | `unknown` carries a cause (§3.3); `/logmon/first_seen` (§3.5) |
| H4 | An fsync under a lock stalls ingest | §3.5's no-lock rule, inherited verbatim from the collector precedent |
| H5 | Archive grows unbounded | v1 is manual-only, so growth is caller-paced. `status.get` reports count and bytes; a WARN crosses a threshold. Real bounding belongs with watches (§9.1) |
| H6 | A domain name is reused and two eras share a registry | Stated (§3.5) and detectable via `/logmon/first_seen` |
| H7 | A case captures a window whose evidence was already evicted | Inherent to a manual path. §4.2 makes it visible, which is the honest answer and part of why watches matter (§9.1) |

---

## 8. Skill and discoverability

Nine tools were proposed; v1 adds **four**. The Store report's finding was that
documentation, not capability, was the binding constraint, and this project has since paid
for that twice — so `skill/logmon.md` gains, in the same change, not afterwards:

- a worked `domain_data` example with a **recommended key set** (`/Versions/*`, `/Action`,
  `/Env/*`), because the whole value is cross-document correlation and two spellings of
  one concept destroy it;
- when to reach for `create_case`, in the "reach for X when…" form the `profile_traces`
  fix used, not a comparison table alone;
- how to read the evidence verdict.

A capability nobody reaches for is the failure this project keeps repeating.

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

---

## 11. Open questions for the gate

1. Per-key volatility hint (`/Action` staleness is minutes, `/Versions/*` is per-deploy).
   Deferred; the format must leave room, since retrofitting a field into a persisted
   contract is expensive.
2. Sidecar format — markdown for reading or JSONL for machines. The bulk is the half most
   likely to be machine-read.
3. Whether `/logmon/first_seen` is sufficient for the reused-domain-name case, or whether
   the registry file should carry a generated id the document copies.
