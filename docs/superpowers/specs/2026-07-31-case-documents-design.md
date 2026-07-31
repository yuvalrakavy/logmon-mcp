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
| **A restart keeps the definition and the history, and discards the live window** | `persist.rs:9-13`: restored *"armed but zeroed — its definition and every recorded run survive, its live window does not."* §3.7's version stamp anchors to **window start**, not to `collectors.add`, because of this row; the draft that anchored to arm time would have warned on every ordinary upgrade. |
| Snapshot provenance is already per-snapshot | `PersistedSnapshot.meta` (`persist.rs:159`) and `CollectorsSnapshot.meta` (`protocol/src/methods.rs:1103`), whose doc comment states the reason: *"per-snapshot because two arms of a comparison differ in it."* §3.8 folds into this rather than adding a parallel store. |

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
a `data` list on `collectors.add` in the same path format (§3.8); a seq range on
`logs.export`.

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
ttl           optional: how long this value stays believable
```

A single "last modified" conflates two questions. Set six days ago and never revisited is
a guess; the same value confirmed five minutes ago is evidence. Invariant:
**`validated_at >= created_at`**.

**`ttl` is optional and per-key**, because staleness is not measured on one clock.
`/Action` is wrong within minutes of switching tasks; `/Versions/*` is wrong on deploy and
fine for weeks otherwise. Without it, the only reader who can judge is one who already
knows which duration to ask about for which key — which the caller writing a document does
not. With it, a document can say *"`/Action` — set 4 minutes ago, past its 2-minute
lifetime"* rather than reporting an age and leaving the judgement unmade.

Three rules keep it from becoming a claim of its own:

- **Absent means unknown, not fresh.** A key without a `ttl` reports its age and no verdict
  — never "current". This is the §4.1 rule; a `ttl` is the only thing that licenses saying
  more than age.
- **Expiry never mutates.** A past-`ttl` key keeps its value and both timestamps; nothing
  is deleted or blanked. The document reports it as expired, and a reader can still see
  what was believed and when — which is the whole point of an archive.
- **`ttl` is advice from the writer, not a fact about the world.** It says how long the
  author expected this to hold. A `/Versions/*` key inside its `ttl` can still be wrong if
  someone deployed; the document therefore says "within its stated lifetime", never
  "verified".

### 3.3 `domain_data.update(entries)`

Each entry is `{path, value?, ttl?}`. **Value present** → set (differs ⇒ both timestamps
move; same ⇒ only `validated_at`). **Value absent** → validate only.

**`ttl` follows the same absent rule as `value`, deliberately.** Absent ≡ `null` ≡
*unchanged* — so a validate-only entry does not silently drop a lifetime the key already
had, and a caller restating a value without repeating its `ttl` does not either. Two
optional fields on one object with opposite absent semantics is precisely the footgun this
section exists to prevent, so there is one rule for both.

**A `ttl` can be changed but not cleared in place.** Clearing means `remove` then re-set.
The alternative is a sentinel, and every candidate is ambiguous — `0` reads as *"stale
immediately"* at least as naturally as *"no lifetime"*, and shipping a value whose meaning
a reader has to look up is how a provenance field starts lying. Un-setting a lifetime is
rare enough to pay two calls for.

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

**The set is the vocabulary for all three lists (§3.8), not just the domain registry.**
`/Build/commit` means the same thing whether it arrives via `domain_data.update`,
`collectors.add(data)` or `cases.create(data)` — only the *moment* differs, and the path
prefix already records that. One vocabulary is what makes the three comparable at all: had
each list invented its own names, the mismatch warnings §3.8 relies on would have nothing
to compare.

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

**Which is why coverage counts all three lists (§3.8), and names the source when it is not
the registry.** A core key satisfied only by `/case/data/Build/commit` still lets the reader
act, so reporting it missing would be false. But a case-supplied value is the capturer's
assertion while a registry one carries a validation timestamp, so the line says so:

> provenance: **3 of 3 core keys** — `/Build/commit` from this capture, not the registry

The alternative — counting only the registry — would push capturers into writing provenance
into `reason` as prose to get it into the document at all, which is the shape §3.6 exists to
prevent.

### 3.7 The reserved namespaces

**The rule: every key/value logmon itself produces lives under `/logmon/`, in every list
(§3.8), and agents cannot write there** — `update`, `remove`, and the `data` parameters of
`collectors.add`, `collectors.snapshot` and `cases.create` all reject the prefix.

Stating it as a rule rather than as the list below is the point. The set logmon knows will
grow; what a reader needs to be able to do forever is glance at a path and know whether a
human asserted it or the broker observed it. Those are different kinds of claim, and §4.1's
whole position is that a document records belief and must say whose.

These keys are **stored**, not computed at document time, so `get_domain_data` can show
what logmon knows, and so an archived document carries what was true rather than what is
true at read time.

**In the domain registry:**

| Key | What it is |
|---|---|
| `/logmon/domain` | the domain name |
| `/logmon/version` | the broker version — `0.10.0` |
| `/logmon/first_seen` | when the *file* was created, the signal that a registry is new, which is what §3.3's `unknown` cause leans on |
| `/logmon/incarnation`, `/logmon/incarnation_started` | §3.5, which is what detects *reuse* — `first_seen` cannot |

`/logmon/version`, not `/logmon/broker_version`: under this prefix the qualifier says
nothing the path has not already said. (`status.get`'s shipped `broker_version` field is
unaffected — that is an RPC result, not a registry key.)

**Logmon's keys carry both timestamps, like everyone else's**, and that turns §3.7's old
special case into ordinary machinery. The draft said `/logmon/version` is "refreshed on
boot, not written once." With §3.2's semantics — `created_at` is *when this value came into
force* — a boot that writes the same version moves only `validated_at`, and a boot after an
upgrade moves both. So the key reads:

> `/logmon/version: 0.10.0` — created 3 days ago, validated 2 minutes ago

which says the daemon has been on 0.10.0 for three days and is running now. Neither fact
was available from a "last modified" stamp, and neither needed a rule of its own.

`/logmon/*` keys do **not** count against §3.1's 256-key cap, or against §3.8's 64-key
per-list cap — otherwise logmon could lock an agent out of its own registry.

**In a collector's list**, logmon contributes `/logmon/version` — **per snapshot, and
anchored to window start for the live window.** The first draft of this paragraph said "at
both arm and snapshot" and claimed the arm-vs-snapshot collision would detect a daemon
upgraded mid-window. That is wrong twice, and the second error is the interesting one:

- A collector restores **armed but zeroed** — "its definition and every recorded run
  survive, its live window does not" (`persist.rs:9-13`). So a definition that predates an
  upgrade would stamp a pre-upgrade version onto a window that is entirely post-upgrade:
  a **false** mid-window-rebuild warning, fired on the most ordinary upgrade path there is.
- And once that is corrected to window start, the two can never differ **at all**. The
  broker version is compiled in, so changing it needs a new process, and a new process
  discards every live window. A warning that cannot fire is worse than no warning: it reads
  as a check.

**The real skew is cross-snapshot, and it is a likelier failure than the one the draft
imagined.** History survives a restart, so one collector's file can hold a run recorded
under 0.9.0 and the next under 0.10.0 — and `collectors.diff` and `collectors.document`
will happily compare them. `PersistedSnapshot.meta` is already per-snapshot
(`persist.rs:159`), which is exactly the right place, and the check is between two
snapshots' `/collector/data/logmon/version` rather than between two ends of one window.
Given this project's release cadence, that comparison is one an agent will actually hit.

**Anything logmon already renders as a first-class field — `level`, `filter`, `wall_ms`,
`matched` — is not duplicated into the list**, or the list becomes a second rendering of
the struct beside the first, with two places to disagree.

**In a case's list, logmon contributes nothing today**, and saying so is better than
inventing a key to fill the row. The capture instant and the trigger are front-matter; the
verdict is its own section; and the broker version at capture cannot differ from the
registry's, because both are read from the running daemon in the same breath. The namespace
exists for when that stops being true.

**`/collector/` and `/case/` are reserved in the *domain registry* only** — rejected by
`domain_data.update`, and legal inside a `data` list. That asymmetry looks inconsistent and
is not: registry keys render with **no** prefix, so a registry key literally named
`/collector/data/Build/profile` would render identically to a genuine collector fact. Inside
a list the prefix is prepended, so `/collector/data/case/x` collides with nothing. The
reservation exists to stop impersonation, and impersonation is only possible where the
rendered path is not already qualified. Cheap now; impossible after the first archive exists.

Per-document facts (capture time, trigger) are **not** registry keys — they are document
front-matter (§5.2). The first draft conflated the two.

### 3.8 Data lists: the same format, at three different moments

The registry answers "what is true of this domain **now**." Two other moments matter to a
document, and each gets its own list in the **same path format** (§3.1 rules verbatim —
leading `/`, ASCII segments, byte-wise comparison, ≤ 256-byte path, ≤ 4 KiB value):

| Supplied on | Renders as | The moment it describes |
|---|---|---|
| `domain_data.update` | `/Build/profile` | the domain, **now**, with `validated_at` (§3.2) |
| `collectors.add(data)` and `collectors.snapshot(meta)` | `/collector/data/Build/profile` | the window that produced **this collector's numbers** |
| `cases.create(data)` and a watch's `data` (§9.1) | `/case/data/Build/profile` | what the capturer asserted **about this document** |

**Two prefixes compose, and they answer different questions.** The outer one — none,
`/collector/data/`, `/case/data/` — says *which moment*. `/logmon/` inside any of them says
*who produced it* (§3.7). So `/collector/data/logmon/version` is the broker version at that
collector's window, and it is unambiguous without a legend: moment, then producer, then the
fact.

**The prefix is the label, and that is the point.** §11's open question 4 asked how a reader
tells `/Build/profile: release` from a domain registry apart from `build_profile: debug` on
a collector arm. The answer is no longer "a warning explains it" — it is that they were
never the same key. Three paths, three moments, and a fact carries its own provenance
instead of leaving the reader to infer it from which YAML block it appeared in. Mismatch
detection survives and gets easier: comparing `/Build/commit` against
`/collector/data/Build/commit` is comparing two well-defined paths.

**Arm-time and snapshot-time merge into one `/collector/data/` list, snapshot winning.**
They bracket the same window — the collector's numbers are produced *between* them — so
splitting them into two namespaces would hand the reader two lists for one measurement,
which is the problem this section exists to remove. `collectors.add` has no `meta` today
(`methods.rs:905`); `collectors.snapshot` does (`:1103`, "per-snapshot because two arms of
a comparison differ in it"). Both now feed one rendered list.

**A key set at both ends with different values is a first-class warning**:
`/collector/data/Build/commit` = `abc` at arm and `def` at snapshot means the window
straddled a rebuild, so the numbers are a blend of two builds. That is not a comparison
problem, it is a corrupted single measurement, and no existing mechanism can see it. Note
what this does **not** cover — the broker's own version cannot move this way, for the
reason §3.7 gives; only facts the agent supplies can.

Every §3.8 mismatch lands in **two** places: a line in the document's Evidence section
(§5.2 orders it before anything it qualifies) and an entry in §5.1's `warnings`, so a
caller scripting over `cases.create` sees it without parsing markdown. Four comparisons are
made, all cheap string compares over paths that already exist at render time:
arm-vs-snapshot within one collector, snapshot-vs-snapshot across a collector's history
(§3.7 — the one that catches a broker upgrade), registry-vs-collector, and
registry-vs-case.

#### Folding `meta` into the list, without pretending it is already this format

`meta` is `Option<Value>` — arbitrary JSON, shipped, and written by agents who have never
read this spec. "Folds in under its own key names" is not a rule until it says what happens
to the shapes registry format does not have:

| `meta` entry | Renders as |
|---|---|
| `"build_profile": "release"` | `/collector/data/build_profile: release` |
| `"runs": 20` (any non-string scalar) | its compact JSON text — matching `meta_field`'s existing `v.to_string()` (`document.rs:1810`) |
| `"config": {"nested": true}` | its compact JSON text **as the value**, not flattened into paths — flattening invents keys the author never wrote, and the path it invents is indistinguishable from one they did |
| `"my key"`, `"héllo"`, `""` — not a legal §3.1 segment | **not folded**, and named in `warnings` |

That last row is the one that matters. Dropping an unfoldable key silently would put logmon
in the position of deciding an agent's provenance did not count, without saying so — the
exact failure §4's every rule exists to prevent. It is also not an error: `meta` never
promised this format, and rejecting the snapshot over it would break a shipped call.

**Two aliases, and the list is closed**: `build_profile` → `/Build/profile` and `git_sha` →
`/Build/commit`. Without them the mismatch check above cannot fire on the two keys the whole
question was about — `/Build/profile` and `build_profile` are different paths under
byte-wise comparison (§3.1), so a registry saying `release` and a snapshot saying `debug`
would sit side by side, uncompared. These two are already privileged by `document.rs:779`,
so aliasing them adds no new privilege. The list does not grow: a general aliasing mechanism
would make two spellings permanently equal and destroy the reason §3.6 has one vocabulary.

**A watch's data has an age; `cases.create`'s does not.** Pairs given to `cases.create` are
supplied at the capture instant and need no timestamp. A watch is armed once and fires for
days, so its data can go stale exactly as a registry key can — armed under `release`, still
firing after a debug rebuild. The document therefore records **when the watch was armed**
beside its `/case/data/`, so those keys carry an age the way §3.2's keys do.

**Caps: 64 keys per list**, against the registry's 256. The registry's cap is justified by
"copied into every never-deleted document"; a collector list has the same property and a
document can embed several of them plus the registry. A lower per-list cap is what keeps
the total bounded. Both `CollectorsDocumentResult.bytes` and §5.1's `CaseFile.bytes` report
the actual size, so a pathological document is visible rather than merely capped.

**`collectors.document`'s promoted `build_profile` / `git_sha` front-matter is unchanged**
(`document.rs:779`). It is a shipped format, and §9.4 of the collector spec argued the
promotion for a reason that still holds. `meta` folds into `/collector/data/` under its own
key names, so those two values appear twice in a `collectors.document` — deliberate, and
safe because it is one source rendered twice rather than two sources that can disagree.

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

`cases.create(reason, anchor, prefix?, data?, before?, after?)` → the document and its two
logdata files, returned **as content**, written by the client (§1). No daemon-side archival writer in v1:
the existing path already writes documents, and adding one would bring file naming,
atomicity, ENOSPC handling and concurrency — all of which belong with watches (§9.1), the
only feature that genuinely cannot use a client.

**`anchor` is explicit** — a seq, a bookmark name, or a `trace_id`. `before`/`after` are
relative to it and size the **logdata**, not the document: the document always shows the
anchor and ±10 (§5.2), so widening the window buys evidence without diluting triage.
§5.2 makes the anchor's message the headline and a front-matter key. An
implicit "newest entry" anchor would headline a case created five minutes after the fact
with whatever happened to arrive last, which fails §5.2's own test that a headline must
distinguish two documents. The first draft's signature had the windows and not the point
they were measured from.

`reason` is **required**. A manual case that cannot say why it exists has no provenance,
and unlike a watch there is no filter standing in for one.

`data` is the §3.8 list, rendered under `/case/data/`. It is optional and its absence is
reported by §3.6.2's coverage line like any other gap — a capturer who knows the seed that
reproduced a flake should not have to write it into `reason` as prose.

**Result shape.** The existing `CollectorsDocumentResult`
(`protocol/src/methods.rs:1410`) carries one optional companion as `sidecar_name` +
`sidecar_content`. `cases.create` needs two, each with its record count, so it gets its own
result rather than a third and fourth `sidecar_*` field:

```rust
pub struct CasesCreateResult {
    pub stem: String,              // "checkout-hang-260731-021530" — §5.3
    pub document: String,          // markdown; write as <stem>.md
    pub logdata: CaseFile,         // write as <stem>.logdata.jsonl
    pub spandata: CaseFile,        // write as <stem>.spandata.jsonl
    pub warnings: Vec<String>,     // what could not be captured (§4.2)
}
pub struct CaseFile { pub content: String, pub records: u64, pub bytes: u64 }
```

`records` is on the wire rather than left for the client to count, because the document
already prints it (§5.2) and two places computing the same number is how they come to
disagree. `stem` is returned rather than reconstructed: §5.3's collision rule runs in the
daemon, so only the daemon knows whether an `<id>` was appended.

**The existing `sidecar_*` fields are not renamed.** They are a shipped wire contract with
shims in the field (the §2 grounding table's first row is that clients write the files),
and `collectors.document`'s companion genuinely is a different thing — a percentile table,
not log records. The rename is this feature's vocabulary, not a migration.

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
7. Pointer to the logdata files, with their record counts.

#### The split, stated as a rule

**The document is what you read to decide whether this is your bug and what to do about
it. The logdata is the evidence you consult once you have decided it is.** The test for
any future field is which of those two jobs it serves.

| In the document | In logdata |
|---|---|
| The anchor entry and **±10 neighbours** — enough to see the shape of the failure | The full pre/post window |
| A **span timing summary** — the slowest few, their durations, the anchor's own trace | Full span trees, every attribute and event |
| Collector numbers at capture (§5.4) | — |
| The registry copy, with ages | — |
| The evidence verdict, and what could not be captured | — |

The ±10 is load-bearing: with only the anchor, every reader must open the logdata to learn
anything and the document stops being a triage surface; with hundreds, the caveats at the
top get scrolled past, which the ordering above exists to prevent.

**Two logdata files, not one, and the name says which:**

```
checkout-hang-260731-021530.md
checkout-hang-260731-021530.logdata.jsonl     ← log entries
checkout-hang-260731-021530.spandata.jsonl    ← spans
```

"Sidecar" named a *role* — companion file — and said nothing about contents. `logdata`
says what it is, and that is exactly why it splits: one mixed stream would make the name a
lie the moment spans went in, and every consumer would filter by record kind before doing
anything. Logs and spans are different shapes with different consumers, and wanting one
without the other is the common case.

JSONL because this half is machine-read — grepped, filtered, streamed — and needs no
whole-file parse. Following `document.rs`'s rule: bulk *moves*, with a pointer and a
record count, rather than being cut without saying so. A file with no records is still
written, empty, rather than omitted: absent cannot be told from "we captured none".

**Not compressed, and that is a decision rather than an omission.** §2's boundary is that
the format is the contract and indexing belongs to whatever walks the directory —
compression puts a codec between the archive and every tool that would do so, and `grep`
over a case archive is the cheapest thing a person can do.

The sizes do not force it. **Measured**, not derived: 200 entries and 50 spans pulled from
this machine's live broker serialise to **507 B/entry** and **238 B/span** as compact
JSONL, so a 700-entry window with its spans is ~**350 KB** and a thousand cases is a few
hundred megabytes. The caveat that number carries: it was measured on *this* broker's
traffic, and record size scales with how many fields the emitting app sets — an app with
fat structured context could be several times larger. It does not change the decision,
because the answer at 350 KB and the answer at 2 MB are the same answer.

Anyone wanting the space back has two routes that cost nothing here — filesystem-level
compression (APFS, btrfs, ZFS) is transparent and keeps `grep` working, and gzipping cold
files later
needs no change to this contract, because compression is a property of storage rather than
of the record format.

### 5.3 Naming

**`<prefix>-<yymmdd>-<hhmmss>[-<id>].md`**, with `<same stem>.logdata.jsonl` and
`<same stem>.spandata.jsonl` (§5.2).

```
checkout-hang-260731-021530.md
checkout-hang-260731-021530.logdata.jsonl
checkout-hang-260731-021530.spandata.jsonl
```

The three share a stem, so the collision rule below covers the set: an `<id>` that
disambiguates the document disambiguates its logdata with it.

**The timestamp is in the name deliberately, and it is the one axis that earns a place
there.** §4.1 argues against encoding a query axis in the layout — but that argument is
about axes where picking one privileges it over equally good alternatives. Time is not one
of many: it is the axis the filesystem already sorts by, for free, with no tooling at all.
Fixed-width `yymmdd-hhmmss` makes lexicographic order chronological order, so `ls` answers
"what happened around then" on an archive nobody has indexed. Metadata queries are about
*content*; this does not compete with them.

**UTC, not local.** Local time makes names unsortable across a DST boundary and
incomparable between machines — and this archive is meant to be shared and read years
later. The name carries second resolution; front-matter carries the precise instant, and
remains the record.

**`<id>` appears only on collision** — two captures in the same second under the same
prefix. Four hex characters, random rather than a counter: a counter has to enumerate the
directory to pick a value, which races another writer, and the client is what writes
(§5.1). The writer checks for an existing file and adds or re-rolls the id.

**Parse from the right.** `safe_name` (`document.rs:1874`) **preserves `-`**, so a natural
prefix like `checkout-hang` survives intact and splitting from the left is ambiguous. The
trailing fields are fixed-shape — six digits, six digits, optional hex — so a tool walking
the archive can recover them deterministically from the end.

**The prefix is sanitised and length-bounded before it reaches a path.** The client writes
`dir.join(name)` with a daemon-supplied name (`cli/collectors.rs:679`), so an unsanitised
caller string is a path-traversal surface — `../../` in a prefix would escape the archive.
`safe_name` collapses everything outside `[A-Za-z0-9_-]`, and the prefix is capped at
48 bytes.

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

**3. Which list to put a fact in** (§3.8), as one sentence per moment rather than a
mechanism description: *what is true of this project* goes in the domain registry once per
session; *what this run was built from* goes on the collector when you arm it; *what you
know about this incident specifically* — the seed, the iteration, the hypothesis — goes on
`create_case`. An agent that puts everything in the registry loses the ability to tell a
measurement's conditions from the current ones, which is the whole reason there are three.

**4. When to reach for `create_case`**, in the bold "reach for X when…" form the
`profile_traces` fix used — a comparison table alone is what left `profile_traces` unused
in the one production trial we have.

**5. How to read the evidence verdict**, including that `filtered` and `partial` are not
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

**`ttl` (§3.2):** a validate-only entry leaves an existing `ttl` intact; restating a value
without a `ttl` leaves it intact; `{path, ttl}` with no value both validates and sets the
lifetime; a past-`ttl` key **keeps its value and both timestamps** and is reported expired
rather than blanked; and a key with no `ttl` reports its **age with no verdict** — never
"current", which is the §4.1 rule the whole field is constrained by.

**Naming (§5.3):** names sort lexicographically into chronological order across a month
and a year boundary; the timestamp is **UTC** even when the host is not, asserted against a
non-UTC `TZ`; a prefix containing `-` (`checkout-hang`) round-trips and the fields still
parse from the right; a prefix with `/`, `..` or a control character is sanitised before it
reaches a path, and one over 48 bytes is truncated rather than rejected; two captures in the
same second under one prefix produce **two files**, the second carrying an id — not an
overwrite, which in a never-deleted archive would destroy evidence silently.

**The recommended key set (§3.6):** coverage names the **missing** core keys, not only a
count — "missing `/Build/commit`" is actionable where "2 of 3" is not; full coverage still
prints the line rather than omitting it, since silence is indistinguishable from a document
that never checked; **zero recommended keys reports `0 of 3`** rather than omitting the
section, that being the case most likely to render as nothing at all. And the
false-positive guard, because a convention hardening into a schema is how this goes wrong:
`update` **accepts a non-recommended key with no warning, no rejection, and no effect on
any other entry's outcome** — coverage is a property of the document, never of the
registry.

**Data lists (§3.8) and `/logmon/` (§3.7):** every writable surface rejects `/logmon/` —
`update`, `remove`, and the `data` params of `collectors.add`, `collectors.snapshot` and
`cases.create`, asserted **per surface**, since a rule stated once and enforced in one
place is the shape this repo has already paid for; `domain_data.update` additionally rejects
`/collector/` and `/case/`, so a domain key can never impersonate a collector-supplied one,
while a `data` list **accepts** those segments, since a prefixed render cannot collide;
`/logmon/version` moves only `validated_at` across a same-version boot and **both**
timestamps across an upgrade, which is the §3.2 semantics that replaced the draft's
"refreshed on boot" special case; `/logmon/*` counts against **neither** cap; **two
snapshots in one collector's history recorded under different broker versions warn**, while
a collector restored across an upgrade and snapshotted once does **not** — that being the
false positive the draft would have shipped, and the one worth a named test rather than a
line in a list; `meta` folding is asserted per row of §3.8's table, including that an
unfoldable key is **named in `warnings` and the snapshot still succeeds**; and the two
aliases fire the mismatch check while a third spelling (`buildProfile`) does not, which is
what keeps the list closed;
`collectors.add(data)` and `collectors.snapshot(meta)` render into **one** `/collector/data/`
list with the snapshot's value winning on collision; that collision **emits a warning in
both places** (§5.2's Evidence line and §5.1's `warnings`) — and the mutation to check is
deleting the comparison, since a warning nothing produces is the failure mode here; a key
present at only one end is **not** a mismatch and warns nothing, which is the false-positive
guard; the same path rules and 4 KiB value cap apply to all three lists, asserted by feeding
each the same malformed key; the 64-key cap is per list, so a document with the registry at
256 and two collectors at 64 is legal; and coverage counts a core key satisfied only by
`/case/data/` as **present, with its source named** — reporting it missing would be false,
and reporting it silently would erase the difference between an assertion and a validated
fact.

---

## 11. Open questions for the gate

1. ~~Per-key volatility hint~~ — **decided**: an optional `ttl` ships in v1 (§3.2), so a
   document can judge `/Action` without the reader knowing which duration to ask about.
2. ~~Bulk-evidence format~~ — **decided** (§5.2): two JSONL files, `.logdata.jsonl` and
   `.spandata.jsonl`, uncompressed. The bulk is the machine-read half, logs and spans have
   different consumers, and compression would put a codec between the archive and `grep`.
3. ~~Whether `/logmon/first_seen` suffices for the reused-domain-name case~~ — **closed**:
   it cannot (monotone), so §3.5 counts incarnations instead.

4. ~~Where build provenance lives~~ — **closed by §3.8**, and by a better mechanism than
   the one proposed here. The draft answer was "keep both and label them"; the truth is that
   they were never one key. `/Build/profile` is the domain now, `/collector/data/Build/profile`
   is the window that produced those numbers, `/case/data/Build/profile` is what the capturer
   asserted. The prefix carries the moment, so reconciliation is a path comparison rather
   than a convention a reader has to know. `document.rs:779`'s promoted front-matter is left
   untouched — it is a shipped format and §9.4's argument for it still holds.

**Nothing is open. Phase 1 is unblocked.**
