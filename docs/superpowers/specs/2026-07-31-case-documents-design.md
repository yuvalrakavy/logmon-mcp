# Domain data and case documents — v1

**Status:** draft for design gate. No code written.
**Tier:** T2 — mints two persisted contracts (the `domain_data` file format and the case
document front-matter) that outlive the code.
**Seams verified at:** `53ebd34`. §1 marks each row **behaviour** (a constructor and a
caller were found) or **intent** (a doc comment or a type, believed and not proven) — see
§1.1 for why that distinction is stated, and why an earlier header claiming *every* row was
behaviour was itself the error §1.1 warns about.
**Supersedes** two earlier drafts of this file (seven lenses, ~90 findings). §9 records what
was cut and why; §12 records what the second gate changed and is the shortest route to
what is different.

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

| Seam | Kind | Evidence |
|---|---|---|
| Per-mutation collector persistence, and its one hard constraint | **behaviour** | `Registry::write` (`collector/registry.rs:399`) → `persist::save` (`collector/persist.rs:401`) → `atomic_write`. Its doc comment (`registry.rs:392-398`) is the constraint §3.5 *considered*: *"`atomic_write` does two `fsync`s … a write under the lock lets one slow fsync stall span ingest daemon-wide."* §3.5 then shows it does **not** transfer here, and why saying it did was an error. |
| A bulk artifact is written by the **daemon** when it is given a path, and returned as content when it is not | **behaviour** | Two shipped shapes, not one. `collectors.document` returns `content` + `sidecar_content` and writes nothing (`mcp/src/server.rs:1046-1089`) — a page of markdown. `export_logs` takes a `path` and writes it (`mcp/src/server.rs:530`) — bulk. §5.1 follows the second, because a case is bulk. An earlier draft cited `:530` as evidence for the *first* shape; it is the counter-example. |
| An entry reaches the store **conditionally**, by **five** paths | **behaviour** | `daemon/log_processor.rs:87-185`: a trigger firing on the entry; a *retroactive* pre-window copy from a later trigger (`:94-99`); a *retroactive* copy sharing a `trace_id` (`:103-111`); an open post-window (`:169-173`); else session filters (`:176-183`). §4.2 exists because of this, and an earlier draft named only the last two. |
| Every session ships two triggers with wide windows | **behaviour** | `engine/trigger.rs:128-158` — `l>=ERROR` and a panic regex, each `pre_window: 500, post_window: 200`, in `Trigger::new()`. So the retroactive paths above fire constantly without anyone adding a trigger. This is what killed the `partial` verdict (§9.4). |
| `logs.export` has no seq range | **behaviour** | `LogsExport { count, filter }` (`protocol/src/methods.rs:282`). Params are read by walking raw JSON (`rpc_handler.rs:692-704`), so §6's additions are absent-safe for existing callers. |
| `safe_name` **preserves** `-` and `_` | **behaviour** | `document.rs:1874-1884`: `is_ascii_alphanumeric() \|\| c == '-' \|\| c == '_'`, everything else to `_`. §5.3's parse-from-the-right rule exists **because** of this. An earlier row here said it "collapses non-alphanumerics", which would have made that rule unnecessary — one function described two incompatible ways in one document. |
| Material-first section order | **behaviour** | `document.rs:839,1068,1156,1349` — `1. What moved` → `2. What to do next` → detail → definitions (then `## 4. The full vocabulary`, `:1293`). |
| Recorded artifacts carry their own definition | **behaviour** | `StoredSnapshot.def` (`collector/history.rs:84`), written by `StoredSnapshot::new` (`:136`) and pinned by `a_snapshot_records_the_definition_it_was_taken_under`. |
| **A restart keeps the definition and the history, and discards the live window — but `window_start` does not know that** | **behaviour** | `persist.rs:9-13` states the intent. `registry.rs:379-381` restores with `Collector::new(def, file.armed_at)` — *"Armed at the ORIGINAL time"* — and `Inner::new` (`state.rs:74`) leaves `zeroed_at: None`, so `window_start()` (`state.rs:98-103`) returns a **pre-restart** timestamp. The signal that does know is on the same struct literal: `zeroed_by: Some("daemon_restart")` (`registry.rs:385`). Two drafts of §3.7 were built on the doc comment instead, and both were wrong. |
| Run provenance already has a home, and this spec does not touch it | **behaviour** | `CollectorsSnapshot.meta` (`protocol/src/methods.rs:1103`) — *"per-snapshot because two arms of a comparison differ in it"* — persisted at `PersistedSnapshot.meta` (`persist.rs:159`), read back by `restore` (`:305`), promoted into per-arm front-matter by `document.rs:780-781`. §3.8 uses it rather than building beside it. |
| A named session's filters come back bound to `default` | **behaviour** | `SessionState.domain` is *"never persisted"* (`session.rs:96-98`); `restore_named` builds via `new_named`, which hardcodes `domain: RwLock::new(DomainId::default_domain())` (`:137`). §4.2's marker must treat a restart as an epoch boundary on **every** domain because of this. |
| One `SeqCounter` feeds both the pipeline and the span store | **behaviour** | `domain.rs:176`. Hence §5.1's windows are record counts, never seq distances, and §6 needs a span read path of its own. |

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
one** (§3.5).

**The rule the rows above follow: a doc comment describes intent and is not evidence of a
code path.** Cite a constructor and a caller, or mark the row *intent*.

The second gate showed the rule is not self-applying. Two rows here were built on
`persist.rs:9-13`, a module doc comment, and both conclusions drawn from it were wrong —
the second one *while fixing the first*. The comment says a restored collector's live
window does not survive; the code keeps the original `armed_at` and never sets `zeroed_at`,
so `window_start()` reports a pre-restart instant and the whole mechanism built on it was a
no-op. And the header of this document asserted that *every* row named a constructor and a
caller at a moment when four did not.

So the failure mode is not "forgot to check." It is **checking the wrong artifact and
reading it as confirmation**, which feels identical from the inside. The `Kind` column
exists so that feeling has to be written down.

---

## 2. Scope

**In:** the `domain_data` registry with its own persistence; `cases.create` (manual, and it
writes the files); a `data` shorthand on `collectors.add` and `cases.create` that is the
same registry call (§3.8), with the `@` sigil scoping a key to one document (§3.9); a seq
range on `logs.export` and a matching span read path (§6).

**Out, deferred to v2:**

- **Watches** — automatic case-writing on a filter match. §9.1 records why that is a design
  problem rather than an implementation one.
- **Cross-checking provenance** — comparing the registry against a snapshot's `meta` and
  warning. §9.5: three of the four comparisons fired on correct usage. v1 renders the
  ordering instead (§3.8), which is a fact rather than a judgement.
- **The `partial` verdict** — §9.4: uncomputable with what the daemon records, and it would
  have appeared on nearly every document if it were not.

**Out permanently:** any query engine over the archive. The format is the contract;
indexing belongs to whatever walks the directory.

**Where it lands, since the draft named no owner.** `domain_data` is a new module in
`crates/core/src/domain_data/`, held by `RpcHandler` alongside domains, sessions and
collectors. Four things change before any feature logic exists, and they are worth knowing
up front rather than discovering: `RpcHandler::new` grows a config-dir argument (two call
sites — `server.rs` and `test_support.rs`); `persistence::quarantine` and
`document::yaml_str` are both private and must be widened; `CollectorRegistry` needs a
by-domain-across-owners lister that carries the owner (§5.4); and four new RPCs mean
`protocol-v1.schema.json` regeneration plus `mcp_tools::TOOLS` and the
`StatusGetResult.broker_tools` list that `tests/capability_skew.rs` checks.

**Phases**, because the draft said "Phase 1" without ever enumerating one:

1. `domain_data` — §3 entire, plus the skill's key-set and when-to-set sections. Ships
   standalone and is useful alone.
2. §4.2's epoch log and §6's two range reads. Both are new daemon recording and neither
   depends on the other.
3. `cases.create` — §5, which needs all of the above.
4. The rest of §8's skill work, the deep gate, and finishing.

The `data` shorthands (§3.8) land with phase 1, since they are the same call.

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

**ASCII is a deliberate narrowing.** It rejects a team naming its own categories in a
non-Latin script — a real cost, taken because keys are correlation identifiers across an
archive and mixed-script keys that look identical are worse than a workaround. **Values are
unrestricted UTF-8**, so borrowed identifiers (image tags, semvers with build metadata,
branch names) are unaffected.

The charset is **close to** `is_valid_name` (`session.rs:187-188`) and not equal to it:
that function allows only `[A-Za-z0-9_-]`, so `.` is new here (wanted for versions and
hostnames), and snapshot labels are documented as a third set, `[A-Za-z0-9._-]`
(`methods.rs:1092`). An earlier draft cited `is_valid_name` as *matching* this rule. It
does not, and the citation was doing the work of a justification.

**Values are escaped on emission, and the two emission contexts have different rules.**
This is where the draft was thinnest: it cited `yaml_str` (`document.rs:1836`) and stopped,
but a registry copy reaches a document twice (§5.2) and only one of those is YAML.

- **Front-matter** uses `yaml_str`, which quotes the scalar and escapes `\`, `"`, `\n`,
  `\r`, `\t`, C0 and DEL. **It does not escape U+0085, U+2028 or U+2029**, which YAML 1.1
  treats as line breaks — and values are unrestricted UTF-8, so a copy-pasted value reaches
  this. Widen the escape set; do not widen the value rule.
- **The body** is Markdown, and no rule existed at all. A value containing a newline ends a
  table row; a value spelling `\n## Evidence\n\nverdict: complete` **forges a section** —
  placed after the real one, which is exactly the failure §5.2's ordering exists to
  prevent, authored by whoever wrote the value. Body values are emitted so they cannot
  begin a line, open a fence, or contain a raw line break.

The first draft constrained keys carefully and left the other half of the same line
unguarded; the second constrained one of the two contexts the value is emitted into.

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

**The literal is a duration string** — `30s`, `5m`, `2h`, `7d`, `4w` — one integer and one
unit, no compounds. The draft shipped `ttl` and never said what one looked like, which
means an agent following the spec sets none, which means every key reports "age, no
verdict" and the feature is unused in the exact way §3.6.2 exists to prevent.

**`ttl` runs from `validated_at`**, and this was undecided in the draft with both answers
producing a false document. From `created_at`, confirmation could never refresh anything: a
key restated daily for a month would read *expired* forever, while §3.2's opening argument
is that a confirmed value is evidence. From `validated_at`, the meaning is the one the
field is for — *how long after someone last confirmed this is it still believable.*

That choice has a consequence, and §8 carries it: a bulk restate at session start **does**
re-arm every `ttl` it touches. That is correct when the agent actually re-derived the
values, and laundering when it copied them forward. So §8 no longer advises restating
everything; it advises restating what was re-read, and validating the rest.

Four rules keep it from becoming a claim of its own:

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
- **A `ttl` can be cleared in place**, with `ttl: false`. The draft said it could not, and
  prescribed `remove` then re-set — which resets `created_at` and so **launders a
  months-old confirmed fact into a fresh-looking one**, the precise thing §3.3's `unknown`
  cause and H3 exist to detect. Two adjacent paragraphs, opposite conclusions.

  The draft's reason was that *"every sentinel candidate is ambiguous."* The repo had
  already answered this, on this exact problem: `CollectorsEdit.threshold` is
  `Option<Option<ThresholdSpec>>` (`methods.rs:1068`), doc-commented *"Pass JSON `null` to
  remove one — which is why this is doubly optional: 'leave it alone' and 'take it away'
  are different requests and a single `Option` cannot express both."* Here `null` is
  already taken (§3.3 defines it as *validate*), so the boolean `false` carries the third
  meaning — but the shape of the answer was in the codebase, and the design argued its way
  past it.

### 3.3 `domain_data.update(entries)`

Each entry is `{path, value?, ttl?}`. **Value present** → set (differs ⇒ both timestamps
move; same ⇒ only `validated_at`). **Value absent** → validate only.

**`ttl` follows the same absent rule as `value`, deliberately.** Absent ≡ `null` ≡
*unchanged* — so a validate-only entry does not silently drop a lifetime the key already
had, and a caller restating a value without repeating its `ttl` does not either. Two
optional fields on one object with opposite absent semantics is precisely the footgun this
section exists to prevent, so there is one rule for both.

**A `ttl` is cleared with `ttl: false`** (§3.2), not by `remove`-then-re-set. The draft
prescribed the latter and it destroys `created_at`, which is the laundering `unknown`'s
cause and H3 exist to detect — two paragraphs of one section reaching opposite conclusions.

**Absent ≡ `null` ≡ validate.** Erasure is `remove`, never `set(null)`. Stated because a
client serialising a missing field as `null` would otherwise mean the opposite — the
absent-vs-zero trap that produced three defects in 0.8.0. Note this is the **opposite** of
`CollectorsEdit.threshold`'s convention in the same daemon (`methods.rs:1068`, where `null`
removes), which is why `false` and not `null` carries the clear.

#### Wire shape, because "per-entry outcomes" was a phrase and not a type

```rust
struct DataEntry { path: String, value: Option<String>, ttl: Option<TtlSpec> }
enum TtlSpec { Duration(String), Clear }        // "30m" | false

struct DataOutcome { path: String, outcome: Outcome }
enum Outcome {
    Created, Updated, Validated,
    Unknown  { cause: UnknownCause },
    Rejected { reason: RejectReason },
}
enum UnknownCause { NeverSet, NoRegistry, Undetermined }
enum RejectReason {
    MalformedPath, PathTooLong, ValueTooLong, ReservedPrefix,
    SigilNotAllowedHere, RegistryFull,
}
```

The draft named five outcomes, gave `unknown` a cause, and said *"rejected rows return
their reason"* without ever enumerating one. An enum is the difference between a caller
that can branch and a caller that string-matches prose.

**`unknown` must not silently create.** Key-only means "confirm what you have"; inventing a
valueless key is the helpful guess that becomes wrong data.

**Its cause is honest about what cannot be determined.** The draft offered `never_set` and
`domain_has_no_registry` as though the two were distinguishable — but `/logmon/first_seen`
is written identically for a brand-new domain and for one whose file was lost or
quarantined, and `/logmon/incarnation` cannot help, since it only moves when a file already
exists. So there is a third case and it is the common one: **`Undetermined`** — no registry,
cause not established. `NoRegistry` is claimed only when a quarantine artifact is present
(§3.5), and that evidence is itself bounded to ten files. Reporting `never_set` for both
would be the confident-shaped guess this document is built against.

**Ordering under the cap.** One malformed entry does not reject the batch: valid rows apply
and rejected rows return their reason. When a batch would cross §3.1's 256-key ceiling,
entries are applied **in the order given** and the overflow is rejected with `RegistryFull`
— not sorted, not all-or-nothing. Stated because it is otherwise unspecified, and two
callers sending the same set in different orders would end up with different registries.

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
  `persistence.rs:214-241`, which already has a boot sweep and a racing-reader test. **One
  gap to close in it:** the directory fsync is `if let Ok(dir) = File::open(parent) { let _ =
  dir.sync_all(); }` (`:237-238`), so both the open failure and the sync failure are
  swallowed — which makes "never silent", two bullets down, not quite true today.

- **A per-domain write mutex, with this lock order:**

  ```
  acquire write mutex → acquire data lock → serialize → release data lock
                      → atomic_write → release write mutex
  ```

  This is the load-bearing rule here, and the draft stated it as *"held across
  serialize-and-write and never across the data lock"* — which has two readings and **the
  natural one is broken**. If the mutex may not enclose the data lock, the snapshot must be
  taken before the mutex; then T1 serializes v1, T2 serializes v2, T2 wins the mutex and
  writes v2, and T1 writes v1 afterwards. **The older lands last** — precisely the failure
  the bullet exists to prevent. The mutex may enclose an acquisition of the data lock; never
  the reverse.

  The collector precedent is safe with no mutex at all because its files are keyed
  `(owner, name)` and have one logical writer; `domain_data` is keyed by **domain**, and many
  sessions bind one domain by design. A lost update here drops the newer key from disk while
  memory keeps serving it — invisible until a restart. §0's failure produced by the
  persistence layer itself.

- Write failure is **logged and named** — "will not survive a restart" — never silent.
- Load quarantines unparseable files with the **numbered** scheme
  (`persistence.rs:268-283`), not `with_extension("json.corrupt")`, which overwrites the
  previous corrupt file. **The numbering is bounded**: `.corrupt` through `.corrupt.9`, and
  the tenth renames over `.corrupt`, destroying the first. H3 leans on a quarantine artifact
  as its only real evidence, so the bound is where that evidence runs out — stated because
  the draft cited this scheme as though it were unbounded.

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

**So the registry counts incarnations, not first sight.** A draft using `first_seen` alone
could not work: it is monotone, so after adoption it still reports era 1, and five unrelated
eras are indistinguishable from one long-lived domain — with the older timestamp reading as
*more* established rather than less.

**The counter is anchored to the SEQ ORIGIN, not to domain creation**, and the difference is
not cosmetic. The draft said "each time logmon creates a domain whose file already exists",
which is wrong at both ends:

- **`default` is created on every boot and its seq never restarts.** It is built by
  `Domain::from_parts` with the daemon's counter seeded from `state.seq_block`, advanced by
  `SEQ_BLOCK_SIZE = 1000` each boot (`server.rs:223-241`). Counting creations would report
  "incarnation 47" on the one domain that was never reused, on the domain most exposed to
  cross-project mixing (H8).
- **Config-declared domains restart seq at 0 on every boot** —
  `Domain::new_with_metrics(config, 0, …)` (`domain_lifecycle.rs:99`), *"empty buffers,
  fresh seq"* — with no re-creation and no name reuse at all. So the hazard H6 describes
  fires without the event the draft keyed on.

So: **increment `/logmon/incarnation` and stamp `/logmon/incarnation_started` whenever a
domain's seq counter is seeded at 0 for a name whose registry file already exists.** That is
the event that makes two seq ranges incomparable, which is the only thing the counter is
for.

**The document records the incarnation beside the seq range**, or its window identifier is
ambiguous across eras. The filename does not (§5.3) — a stated limit, not an oversight.

### 3.6 The recommended key set

A registry whose keys are freely invented is a registry that cannot be correlated. Under
time pressure one person writes `/version`, another `/Versions/server`, and six months on
there is nothing to group by — which defeats the only reason the archive is global (§4.1).
So the set is **defined here, documented in the skill (§8), and reported against (§3.6.2)**.

It is a **convention, not a schema**: any path is still legal, nothing is rejected for
being absent, and a project with a concept logmon never anticipated just adds a key.

**The set is the vocabulary of the registry, however a key arrives** — `domain_data.update`
and `collectors.add(data)` are the same call (§3.8), so `/Build/commit` means one thing and
has one spelling. That is what makes an archive correlatable: `grep -l '/Build/commit: 9f2a1c'`
over a case directory answers "which incidents ran this code", and it answers it only if
nobody invented a second spelling.

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

> provenance: **2 of 3 core keys** — missing `/Build/commit`. Present: `/Build/profile`
> (4d), `/Action` (12m). Contextual: `/Env/host` (4d), `/Data/seed` (12m).

That makes the convention self-enforcing at the moment it matters, without rejecting a
single legitimate key. It is the same rule as the rest of §4: absence stated as plainly as
staleness, because a reader cannot tell "not recorded" from "not applicable" unless the
document says which.

**Coverage counts the registry only, and every key carries its age on the same line.** Two
corrections the gate forced, both of which the draft's shorter line had wrong:

- **Ages, not a bare count.** A `/Build/commit` set six months before capture and never
  revalidated counted toward "3 of 3" with nothing saying so, in a line whose whole job is
  to report on provenance quality. §3.6.2 claimed to state *"absence as plainly as
  staleness"* while exhibiting neither.
- **Named contextual keys, not a tally.** `/Versions/<component>` is an open family, so "4
  contextual keys present" cannot be compared against another document's "6" — one project's
  four components and another's six are not a measurement of anything. Naming them costs a
  line and means something.

`update` never rejects a non-recommended key, and never warns. Coverage is a property of
the *document*, not of the registry.

### 3.7 The reserved `/logmon/` namespace

**The rule: every key/value logmon itself produces lives under `/logmon/`, and agents cannot
write there** — `update`, `remove`, and every convenience writer into the registry (§3.8)
reject it.

Stating it as a rule rather than as the list below is the point. The set logmon knows will
grow; what a reader needs forever is to glance at a path and know whether a human asserted
it or the broker observed it. Those are different kinds of claim, and §4.1's whole position
is that a document records belief and must say whose.

**The reservation is over SEGMENTS, in both directions.** A path is reserved iff its first
segment is `logmon`. A `remove` pattern is rejected iff it matches *or is matched by* a
reserved path. Both halves are load-bearing and the draft had neither:

- `/logmon` — no trailing slash — is a perfectly legal §3.1 path that a `starts_with("/logmon/")`
  guard **accepts**. An agent could seat a value there, rendered unprefixed beside the
  broker's own keys, defeating this section's entire purpose. And it would be
  **unremovable**, because `remove` rejects patterns matching the reserved subtree.
- Read the other way, `remove(["/logmon"])` under §3.1's segment-boundary matching **wipes
  every reserved key at once**, and §3.1 says `remove` has no undo. That deletes
  `first_seen` and `incarnation` — which is to say it deletes H3's and H6's entire defence,
  silently, from a call that looks like tidying up.

These keys are **stored**, not computed at document time, so `get_domain_data` can show what
logmon knows, and so an archived document carries what was true rather than what is true at
read time.

| Key | What it is |
|---|---|
| `/logmon/domain` | the domain name |
| `/logmon/version` | the broker version |
| `/logmon/first_seen` | when the *file* was created |
| `/logmon/incarnation`, `/logmon/incarnation_started` | §3.5 — what detects seq-origin reuse |

`/logmon/version`, not `/logmon/broker_version`: under this prefix the qualifier says
nothing the path has not. (`status.get`'s shipped `broker_version` field is unaffected —
that is an RPC result, not a registry key.)

**Logmon's keys carry both timestamps, like everyone else's**, which turns the draft's
special case into ordinary machinery. It said `/logmon/version` is "refreshed on boot, not
written once." With §3.2's semantics — `created_at` is *when this value came into force* —
a boot writing the same version moves only `validated_at`, and a boot after an upgrade
moves both:

> `/logmon/version: 0.9.0` — created 3 days ago, validated 2 minutes ago

The daemon has been on this version for three days and is running now. Neither fact
survived a "last modified" stamp, and neither needed a rule of its own.

**One caveat this creates, stated because §3.5 permits the failure:** a boot-time write can
fail and is logged rather than fatal. The registry then reports the previous version. So
`/logmon/version` is *what the registry last recorded*, not *what is running* — which is
why `status.get` remains the authority for the live version and the two are not
cross-checked (§3.8's rule against comparing facts that legitimately differ).

`/logmon/*` keys do **not** count against §3.1's 256-key cap — otherwise logmon could lock
an agent out of its own registry.

**Per-document facts are not registry keys.** Capture time, trigger, the anchor, and
whatever the capturer asserts about *this incident* are document front-matter (§5.2). The
first draft conflated the two, and §3.8 explains why the second draft conflated them again
in a different direction.

### 3.8 One registry, and two conveniences that write it

**There is exactly one key/value store: the domain registry.** `collectors.add` and
`cases.create` each take a `data` parameter, and both are defined as
**`domain_data.update(entries)` on that call's domain** — the same validation, the same
per-entry outcomes, the same `/logmon/` guard, the same caps, one implementation. Not a
namespace, not a parallel store, not a new concept, and not three semantics behind one
spelling.

The one thing a caller may vary is **scope**, per key, with the `@` sigil (§3.9) — and that
is a property of the key rather than of the call, so the rule stays learnable: *a `data`
list writes the registry, unless the key says otherwise, and then the key says so in the
document too.*

#### Why this section replaced a three-namespace design

The draft gave collectors and cases their own subtrees — `/collector/data/…`,
`/case/data/…` — so that a fact could carry the moment it was true. A four-lens gate found
the mechanism built on top of that unsound in ten separate ways, and every one of them
existed only because the subtrees were a *second* key/value mechanism beside the registry:
a fold rule for `meta`, two aliases, a merge rule, four cross-checks, three caps, and a
reservation that could not be enforced on the one surface that needed it.

The deeper error is worth naming because it recurs. **`collectors.snapshot(meta)` was
already the answer to "what was this run built from."** It is per-snapshot by construction
(`methods.rs:1103`: *"per-snapshot because two arms of a comparison differ in it"*), it
persists (`persist.rs:159`), and `document.rs:780-781` already promotes `build_profile` and
`git_sha` out of it into per-arm front-matter. A parallel mechanism was designed beside a
working one, and the gate spent most of its findings on the seam between them.

**`meta` is therefore untouched by this spec.** No folding, no aliases, no merge. It keeps
its shape, its free-form JSON, and its existing rendering.

#### Where each kind of fact goes

| Fact | Where | Why |
|---|---|---|
| What is true of this project — commit, profile, versions, host | **registry**, via `domain_data.update`, `collectors.add(data)` or `cases.create(data)` | Domain state. One current value, with `created_at` saying since when. |
| What *this run* was built from | **`collectors.snapshot(meta)`** — unchanged | Per-snapshot, persisted with the numbers, already rendered. |
| What the capturer asserts about *this incident* — the seed, the iteration, the hypothesis | **`cases.create(data)` with an `@` key** (§3.9) | Same call, same vocabulary; the sigil scopes it to the document. |

#### Why the shorthands exist at all

They are shorthands, and a shorthand needs a reason. The reason is §3.6.2's: a convention
that lives only in documentation is a capability nobody reaches for. Recording provenance at
the moment the agent is already thinking about the run — arming a collector, writing up an
incident — costs one parameter; recording it in a separate call costs a *decision*, and the
decision is what does not get made.

They are shorthands and nothing more. Both return the same per-entry outcomes
`domain_data.update` returns, so an agent learns one vocabulary. `collectors.add(data)` is
not persisted with the collector, does not appear in `CollectorDef`, and is not affected by
`collectors.edit` — there is nothing to persist there, because the registry already did.

**`cases.create(data)` is applied before the document's registry copy is rendered**, so a
key supplied at capture appears in *that* document rather than first showing up in the next
one. Ordering that would otherwise be arbitrary, and silently wrong in the direction of a
reader concluding the capturer recorded nothing.

### 3.9 The `@` sigil — one list, two scopes

**A `data` key beginning `@` is written to the document and not to the registry.** Strip the
sigil and what remains is an ordinary §3.1 path, so the vocabulary is shared: `@/Data/seed`
and `/Data/seed` are the same fact at two scopes, not two spellings of one.

```
data: [{path: "/Env/host",    value: "ci-7"},   → registry: true of the domain
       {path: "@/Data/seed",  value: "8814"}]   → this document only
```

**Why a sigil rather than a second parameter or a second tool.** The scope of a fact is a
property of the fact, and the agent recording it is the only party that knows which it is.
A tool-level split forces the caller to sort their knowledge into two calls before they know
the sorting rule; a key-level marker lets them write what they know and mark the one thing
that is local. It also means the sorting rule can be stated in one line in the skill, which
§3.6.2 says is the binding constraint on whether any of this gets used.

**`@` specifically**, for two reasons that are not aesthetics: it cannot appear inside a
§3.1 path, so `@/x` is unambiguous and needs no escaping; and it is not special to any
common shell, unlike `!`, `*`, `?`, `~`, `$`, `&` and `#`, which matters because these keys
get typed at a CLI.

**The sigil survives into the document.** `@/Data/seed` renders with its `@`, so a reader
sees the scope in the key and needs no legend, and `grep '/Data/seed'` over an archive is a
different question from `grep '@/Data/seed'` — one asks what domains ran that seed, the
other asks which incidents claimed it. Rendering it bare would merge two questions that the
sigil exists to separate.

**Three rules keep it from becoming a loophole:**

- **`@/logmon/…` is rejected.** A sigil scopes a key; it does not launder the reservation
  (§3.7). The same for any future reserved prefix.
- **A sigil key on `domain_data.update` or `collectors.add` is rejected, with a message
  naming why** — neither has a document to put it in, and silently dropping it would be
  logmon deciding an agent's provenance did not count without saying so.
- **`@/x` and `/x` are different keys** and do not overwrite each other. A capturer may
  legitimately record both: the domain believes one thing, this incident asserted another,
  and flattening them would destroy exactly the distinction the sigil was added for.

**Caps and coverage.** Sigil keys do not count against §3.1's 256-key registry cap — they
never reach it — and get their own cap of 64 per document, counted against §5.2's rendered
budget. For §3.6.2's coverage line, a core key satisfied only by a sigil **counts as
present and is named as asserted**: it does let the reader act, so reporting it missing
would be false, and the sigil is already in the rendering, so no separate "source" column is
needed to keep the reader honest about what it is.

#### No cross-checks. Render the ordering instead.

The draft compared the registry against a collector's provenance and warned when they
differed. Three of the four comparisons fired on **correct usage**, because the design's own
thesis is that these are different facts at different moments:

- Registry-vs-collector fires on every deploy — armed Monday under `abc`, registry updated
  Wednesday to `def`, nothing wrong. §5.2 puts Evidence *before anything it qualifies*, so
  the loudest line at the top of the document would be an alarm produced by the design
  working.
- Arm-vs-snapshot fires on the flagship A/B gesture, because `CollectorsSnapshot.reset`
  defaults to `true` (`methods.rs:1107-1109`) and `registry.rs:694-701` swaps the window. A
  second snapshot's window begins at the first, entirely inside the new build; comparing it
  to arm time reports a rebuild it never straddled.

**So the document renders the ordering, which is a fact rather than a judgement and cannot
false-positive:**

> `/Build/commit: def` — in force since 2 days ago
> collector `checkout` — snapshot taken 5 days ago, `git_sha: abc`

A reader sees that the numbers predate the current value without anyone asserting that is
an error. Every input to that line already exists: the registry's `created_at` (§3.2) and
the snapshot's `taken_at` (`persist.rs:160`).

**The general rule, which the gate paid for:** where two records legitimately describe
different moments, render both with their moments and let the reader compare. A warning is
a claim that something is wrong, and it may only be emitted where wrongness is what was
actually detected.

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

**Four verdicts, and `complete` is claimable only by proving a negative:**

| | |
|---|---|
| `complete` | the range lies wholly inside an unfiltered epoch, **and** no eviction was detected, **and** nothing was capped, **and** the store was non-empty for the range |
| `evicted` | the ring dropped part of the range |
| `filtered` | a session filter was narrowing the store for some of the range — **which filters, and over which seqs** |
| `cannot_verify` | the marker has no epoch covering the range, or the store is empty, or a restart falls inside it |

**`cannot_verify` is a first-class verdict, not prose.** The draft named it only in a
paragraph thirty lines below the table, so every clause of the normative `complete` row was
satisfied by an empty store and an implementer following the table would have shipped
exactly the defect the paragraph existed to prevent. `evicted_before_window` returns `None`
when `buffer_oldest_seq` is `None`, so a capture after `clear_domain` reads as *0 entries,
complete* unless emptiness is a verdict of its own.

**A point-in-time filter read cannot compute any of this.** The first draft said "record
the domain's active filter set at capture" — but that is a snapshot of a time-extended
property, and routine events empty it silently: an anonymous session is **removed** on
disconnect and takes its filters with it (`session.rs:271-279`), `remove_filter`, and
`set_domain` re-binding a session out of the domain. Session A filters, the run happens, A
disconnects; agent B captures and sees no filters, so the document asserts "no filter was
narrowing the store" over a window that recorded a tenth of it.

#### The narrowing marker, and the six ways the draft's enumeration was short

The daemon maintains a **per-domain epoch log**: each entry is `(seq_at_flip, filtered:
bool, filter_strings)`, stamped whenever *does this domain narrow storage* could change.
`complete` is claimable only for a range lying wholly inside one unfiltered epoch.

The draft listed three flip sources — `filters.add/remove/edit`, `set_domain`, session
disposal — and called the list closed. It is a closed-world claim, so being short is not a
gap but a **false `complete`**. What it missed, each verified:

| Missing source | Why it flips, and what the draft would have claimed |
|---|---|
| **Daemon restart** | `SessionState.domain` is *"never persisted"* (`session.rs:96-98`) and `restore_named` hardcodes `default` (`:137`). A named session filtering `web` comes back filtering **`default`** — so after every restart `default` is narrowed by filters its user never wrote, and `web` silently reverts to store-all. **A restart is an epoch boundary on every domain**, which is also why it forces `cannot_verify` for any range straddling it. |
| **Filters that no longer parse** | `if let Ok(condition) = parse_filter(…)` (`session.rs:1005`) drops them on restore. If 3 of 4 survive there is no boolean flip, so a flip-only marker keeps naming a filter that is gone — and §4.2 promises *"which filters, and over which seqs."* |
| **`filters.edit`** | `session.rs:673-710` replaces the condition in place; the boolean never moves. A domain filtered by `l>=ERROR` over seqs 0–100 and edited to `service:auth` for 100–200 would be reported as *"filtered by [service:auth] over 0–200"* — **wrong information rather than missing information**, which is §0's failure. The marker therefore stamps on any change to the filter *strings*, not only to the boolean. |
| **`session.rename` displacing a stale holder** | `session.rs:342-354` destroys a disconnected session and its filters. |
| **`domains.delete` not rebinding sessions** | `rpc_handler.rs:384-406` leaves `state.domain` naming the dead domain, so re-creating that name instantly re-applies those filters to the fresh instance. |
| **Disconnected named sessions still filter** | `evaluate_filters_for_domain` (`session.rs:902-905`) checks only `is_in_domain` — **no `connected` check**, unlike the trigger scan at `:939-942`. So keying the marker on *connected* sessions produces a false `complete` for precisely H2. The TTL sweep (`server.rs:585-598`) is the only thing that ever retires them, and it is the flip that matters. |

**One ordering rule, because the marker and the decision run on different threads.** The
stamp takes `pipeline.current_seq()` on the RPC thread while the log processor assigns seqs
at `log_processor.rs:53` and decides storage at `:168-185`. Unless the two share a sequence
point, the epoch boundary is approximate by the in-flight depth and `complete` is claimed
over entries evaluated under the other policy. The flip is therefore applied **inside the
same lock the processor takes to decide**, not stamped beside it.

**Completeness is a property of the storage POLICY over a range, never of the entries in
it.** The draft said it was defined over *"stored-entry provenance"* — but stored-entry
provenance is `LogEntry.source` (`gelf/message.rs:84-88`), which explains why a **kept**
entry was kept. **No stored entry can testify about an absent one.** Only a time-extended
record of the policy can, which is what the marker is.

**Two further sources of false `complete`, both closed at the source:**

- **A capped export satisfies "asked N, got N".** `recent_with_scanned` walks
  newest→oldest (`memory.rs:58-101`), so `count` drops the **oldest** entries — the context
  *before* the anchor, which is the half that matters. §6 therefore caps outward from the
  anchor and reports `capped` explicitly on the wire; a capped result is never `complete`.
  It must be explicit because comparing requested against returned cannot distinguish
  "capped" from "exactly N existed", and `LogsExportResult.truncated`
  (`rpc_handler.rs:719-733`) is computed only from the lower bound and knows nothing about
  the `count` cap.
- **Seq is shared with spans.** One `SeqCounter` feeds both the pipeline and the span store
  (`domain.rs:176`), so a 200-seq range holding 160 spans returns 40 logs with nothing
  missing.

**The verdict covers the logs.** `.spandata.jsonl` gets its own line stating what was
captured and whether the span ring had evicted below the range — the span store has its own
ring with its own eviction, and one verdict cannot honestly speak for two stores (§6).

## 5. `cases.create` — the manual path

### 5.1 Shape

```
cases.create(reason, anchor, dir, prefix?, data?, before?, after?)
```

**`dir` is required, and the daemon writes.** The draft had neither, and the gate found the
two halves of that mistake independently:

- The draft returned all three files **as content** over RPC. `collectors.document`'s
  precedent (§1) genuinely is client-writes — but its content is a page of markdown, while a
  case carries ~400 KB of JSONL (§5.2). Returning that through an MCP tool result puts the
  entire archive into the model's context, which is precisely the outcome §5.2's split
  exists to prevent. The one shipped MCP tool that writes a bulk artifact, `export_logs`
  (`mcp/src/server.rs:530`), takes a path for exactly this reason.
- The draft then assigned §5.3's collision check to the daemon (*"only the daemon knows
  whether an `<id>` was appended"*) while §5.3 assigned it to the client. **Neither could
  do it**: the daemon had no directory, and the client could not re-roll a stem the daemon
  had already rendered into the document's own front-matter and its logdata pointers. Two
  agents capturing in the same second would silently overwrite each other — the one outcome
  §10 requires cannot happen.

Both dissolve with a `dir` parameter. The daemon resolves the collision because it can now
read the directory, and the artifact it renders is the artifact that lands.

**A relative `dir` is rejected, not resolved.** `cli/collectors.rs:654-656` states why:
*"the broker runs as a service, so a relative path would resolve against its working
directory rather than the caller's."* Silently writing to the daemon's cwd is the failure
mode; refusing is one line.

**`anchor` is explicit** — a seq, a bookmark name, or a `trace_id`, as a **tagged** value
rather than one sniffed string. Sniffing misfires on a bookmark named `12345` or one
spelled as 32 hex characters, and the failure is silent: you get a document anchored
somewhere else. §5.2 makes the anchor's message the headline, so a wrong anchor is a wrong
document, not a wrong parameter.

An **unresolvable anchor is an error, not a degraded document.** This is the one place §4.2's
emit-and-state philosophy does not apply: a document whose headline cannot identify the
incident fails §5.2's own test of what a headline is for, and §9.2 made the headline
mandatory when `cases.find` was cut. A `trace_id` resolving to many entries takes the
**earliest by seq** as the anchor and says so in the headline.

`before`/`after` are **counts of stored records**, relative to the anchor, and they size the
**logdata** rather than the document — the document always shows the anchor and ±10 (§5.2),
so widening the window buys evidence without diluting triage. They are counts and not seq
distances because §4.2's own trap says why: one `SeqCounter` feeds both stores
(`domain.rs:176`), so a 200-*seq* range holds an unpredictable number of logs. Default 350
each way; `before` and `after` are separately capped at 5 000, because the transport buffers
the whole capture and `logs.export`'s `count` already defaults to `u64::MAX`
(`rpc_handler.rs:695`).

**`before`/`after` size the logs. Spans are scoped by the resulting seq range** — see §6 for
the read path, which is new work the draft assumed existed.

`reason` is **required**. A manual case that cannot say why it exists has no provenance,
and unlike a watch there is no filter standing in for one.

**`data` is `domain_data.update`, exactly as it is on `collectors.add`** (§3.8) — one list,
one vocabulary, one implementation. Keys carrying the `@` sigil (§3.9) go to the document
instead, which is how a capturer records something true of *this incident* rather than of
the domain.

**Every fact in the document is dated against two instants, and both are rendered**, because
they differ and the difference is the one §5.1 already argues about when it rejects an
implicit anchor: a case written after the fact is about the incident, not about now. A flake
that reproduced at 14:00, was investigated, and was written up at 14:20 produces a registry
copy as-of 14:20 over an anchor at 14:00, so:

> `@/Data/seed: 8814` — recorded at capture, 13m after the anchor

A reader can then decline to tie a fact recorded at 14:20 to something that happened at
14:00. An earlier draft said case data *"needs no timestamp"* because it is *"supplied at
the capture instant"* — true, and irrelevant, because the capture instant is not the
incident instant.

**Result shape.**

```rust
pub struct CasesCreateResult {
    pub stem: String,              // "checkout-hang-260731-021530" — §5.3
    pub paths: Vec<String>,        // what was actually written, in order
    pub verdict: String,           // §4.2: complete | evicted | filtered | cannot_verify
    pub document_bytes: u64,
    pub logdata: CaseFile,
    pub spandata: CaseFile,
    pub notes: Vec<CaseNote>,
}
pub struct CaseFile { pub records: u64, pub bytes: u64 }
pub struct CaseNote { pub kind: String, pub detail: String }
```

`document_bytes` sits beside them for the reason §5.2 gives: the document is the artifact
whose size can run away, and the draft measured only the two that could not.

Three things the draft got wrong here, all found by the cold-reader lens:

- **The verdict is on the wire.** The draft left §4.2's verdict — which §4.2 itself calls
  *"the document's most important correctness property"* — reachable only by parsing
  markdown, in a spec that justified `records` on the wire with *"two places computing the
  same number is how they come to disagree."*
- **`notes` is typed, not `Vec<String>`.** The draft multiplexed capture gaps, provenance
  observations, and write problems into one prose list, then claimed a caller could act on
  it *"without parsing markdown."* A `kind` is what makes that true.
- **No `document: String`.** The daemon writes; returning the body as well would double the
  transport and re-open the context problem `dir` was added to close. `paths` says what
  landed.

**`collectors.document`'s `sidecar_*` fields are unchanged.** They are a shipped wire
contract with shims in the field (§1's second row), and its companion genuinely is a
different thing — a percentile table, not log records. This feature's vocabulary is not a
migration of that one.

### 5.2 Structure: discriminating first, then qualifying, then bulk

**Front-matter is the index surface, and it is fixed-schema and small** — the case id, both
instants, the domain and incarnation, the reason, the anchor, the headline, the verdict, the
seq range, the two file pointers with counts, the core-key coverage summary, and any `@`
assertions (§3.9). **Not the whole registry**: §3.1 permits 1.1 MB of it, and a megabyte
above the fold defeats the one thing front-matter is for. §11 exhibits it.

Then, in the body:

1. **Headline** — the anchor entry's own message, the time, the domain. It must identify
   *this* incident: a headline that cannot distinguish two documents is not an index.
2. **Evidence** — §4.2's verdict, the span line, and provenance with ages. Before anything
   it qualifies; a caveat reached after 400 lines has already failed.
3. **What to do next** — the capturer's guess. `document.rs`'s house order has this at
   position 2, and this document departs from it deliberately, because §4.2's verdict can
   invalidate the guess and a reader who acts before reading it has been misled by layout.
4. **Anchor entry and neighbours.**
5. **Provenance** — the registry copy, each key with its age. This is the **only** place
   the registry is rendered in full; front-matter carries the core keys alone.
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

**Every file carries a format version, and the archive is the reason.** The first line of
each JSONL file is a header record — `{"logmon_format": 1, "kind": "logdata"}` — and
front-matter carries the same integer. §2 rules out any query engine, so the format *is* the
contract; add a field to `LogEntry` in 0.12 and an unversioned archive holds two record
shapes with nothing distinguishing them. The collector path already has `FORMAT_VERSION: u32
= 1` (`persist.rs:31`) for exactly this, and the draft inherited none of it. `/logmon/version`
in the registry copy is a proxy and not a contract — §3.7 shows it can be stale.

**Not compressed, and that is a decision rather than an omission.** §2's boundary is that
the format is the contract and indexing belongs to whatever walks the directory —
compression puts a codec between the archive and every tool that would do so, and `grep`
over a case archive is the cheapest thing a person can do.

The sizes do not force it. **Measured**, not derived: 200 entries and 50 spans pulled from
this machine's live broker serialise to **507 B/entry** and **238 B/span** as compact JSONL.
So a 700-entry window is ~**355 KB** of logdata, and at the sample's own 4:1 ratio its ~175
spans add ~42 KB — call it **~400 KB of logdata per case**, and a few hundred megabytes for
a thousand.

Two caveats that number carries, the second of which the draft got wrong:

- It was measured on *this* broker's traffic. Record size scales with how many fields the
  emitting app sets, so an app with fat structured context could be several times larger.
- **It is the logdata only.** The draft quoted ~350 KB as if it were the case, and the
  document is a separate and separately-bounded artifact (below). Planning archive capacity
  from the logdata figure undercounts.

Neither changes the decision, because the answer at 400 KB and the answer at 2 MB are the
same answer.

Anyone wanting the space back has two routes that cost nothing here — filesystem-level
compression (APFS, btrfs, ZFS) is transparent and keeps `grep` working, and gzipping cold
files later needs no change to this contract, because compression is a property of storage
rather than of the record format.

#### The document has its own bound, because §3.1's caps do not give it one

§3.1 permits 256 keys × 4 KiB — **1.1 MB of registry** — and the document renders the
registry. Nothing in the draft stopped a triage surface being three times the evidence it
points at, in a section whose own rule is that the document is what you read *first*.

- **The registry appears once**, in the body Provenance section, with ages. Front-matter
  carries the *core* keys only (§3.6.1's three), because front-matter is the grep surface
  and a megabyte of it defeats that. The draft listed the registry copy in both and said
  nothing about the duplication.
- **The rendered registry is capped at 64 KB.** Past that, keys are rendered newest-validated
  first and the remainder becomes a count and a pointer to the front-matter — following
  §5.2's own rule that bulk *moves* with a pointer rather than being cut in silence.
- **`CasesCreateResult` reports the document's bytes** along with each logdata file's, so
  the pathological artifact is the one that is measured. The draft reported bytes for the
  two files that could not be pathological and not for the one that could.

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
directory to pick a value, and two writers enumerating concurrently pick the same one.

**The daemon resolves it, because §5.1 gives the daemon the directory and the write.** The
draft split this — §5.1 said the daemon knew whether an `<id>` was appended, §5.3 said the
client checked — and **neither could do it**: the daemon had no directory, and a client
re-rolling the stem would be renaming a file whose own front-matter and logdata pointers the
daemon had already rendered under the old name. Two agents capturing in the same second
would have silently overwritten each other, which §10 requires cannot happen.

Resolution is **create-new-exclusive, then re-roll** — `OpenOptions::new().create_new(true)`
on all three paths, re-rolling the id on `AlreadyExists`, up to 8 attempts before failing
the call. Not "check then write": between a check and a write another process can land, and
in a never-deleted archive the loser's evidence is gone with no error anywhere. The
filesystem's own exclusivity is the only check that does not race.

**Parse from the right.** `safe_name` (`document.rs:1874`) **preserves `-`**, so a natural
prefix like `checkout-hang` survives intact and splitting from the left is ambiguous. The
trailing fields are fixed-shape — six digits, six digits, optional hex — so a tool walking
the archive can recover them deterministically from the end.

**The prefix is sanitised and length-bounded before it reaches a path.** The daemon writes
`dir.join(name)` (`cli/collectors.rs:685` is the shipped shape), so an unsanitised caller
string is a path-traversal surface — `../../` in a prefix would escape the archive.
`safe_name` collapses everything outside `[A-Za-z0-9_-]`, and the prefix is **truncated**
to 48 bytes rather than rejected, on a UTF-8 boundary.

**`prefix` defaults to `case`** when omitted. The draft left the optional parameter with no
default, which is the common path: an absent prefix would have produced `-260731-021530.md`
— a leading-dash filename that most CLI tools read as a flag — or `260731-021530.md`, which
parse-from-the-right cannot separate from a six-digit prefix.

**The name carries no domain and no incarnation, and that is a real limit.** §3.5 argues
that a domain name is not an identity and that a document must record its incarnation
beside its seq range — but the *filename* carries neither, and the filename is the only
index §2 permits. `ls` cannot separate two eras or two domains; you open the file. The
alternative — encoding either into the name — re-privileges a query axis the way §4.1
argues against, for a discrimination that front-matter already makes. Stated here so a
future reader knows it was weighed rather than missed.

### 5.4 Collector state at capture

Include each collector's current numbers and any snapshot it holds. Two constraints, from
different places — the draft attributed both to §1's lock rule and only the first is about
a lock:

- **A lock constraint.** Projection is computed **outside the registry lock** — it sorts
  every retained duration, which under the lock would stall ingest. This mirrors the
  existing three-step split in `snapshot` (`registry.rs:677-720`).
- **A selection constraint.** **Which collectors** are visible must be decided, not assumed.
  `Registry::list` filters by **owner only** (`registry.rs:617`) and never by domain, while
  `ArmedCollector.domain` is a pin that a later `use_domain` does not move. So "the calling
  session's collectors" is wrong twice: it can embed collectors measuring a *different*
  domain, and it misses collectors armed on *this* domain by anyone else — the CLI connects
  as session `cli` while the shim uses its own, so an MCP-created case would see none of the
  CLI's. v1 therefore selects **by the case's domain, across owners**, via `owners()`
  (`registry.rs:871`) + `list(owner)` filtered on `domain`.

  **Naming the owner is new work, not a read.** `ArmedCollector` (`registry.rs:111-123`)
  has no `owner` field — `Entry::armed()` drops it — so the selection needs a lister that
  carries it. The draft said "names the owner beside each collector" as though the field
  were there.

If no collectors are armed **on this domain**, the section says so in those words.
Omission reads as "nothing interesting", and the first draft's session-scoped wording would
have printed a claim about the domain that was false.

---

## 6. Range reads: a seq range on `logs.export`, and a span read path that does not exist yet

### 6.1 `logs.export`

Add `from_seq` / `to_seq`. Absent-safe (§1).

**They are lowered into `SeqFilter` qualifiers before resolution, not carried as loose
params.** `evicted_before_window` reads its lower bound out of the *parsed filter* via
`resolved_lower_bound` (`filter/parser.rs:745-773`), which matches `Qualifier::SeqFilter` —
today produced only by `b>=` / `c>=` resolution. A top-level `from_seq` would never reach
it, so a range whose start had already rolled out of the ring would come back
`truncated: false` and §4.2 would read that as `complete`. Lowering makes them compose with
a bookmark bound through the existing max-of-lower-bounds rule and feeds the detector for
free.

**`from_seq` and `to_seq` are INCLUSIVE, lowered to `Gt(from-1)` / `Lt(to+1)`.** The draft
said only *"`SeqFilter::Gt` is strict"*, which describes the internal op and leaves the
parameter undecided — and `SeqOp` has exactly two variants, `Gt` and `Lt`
(`parser.rs:58-62`), because bookmark semantics are strict by design. The consequence of
leaving it: with `before = 0` and `after = 0` a strict range **excludes the anchor entry
itself**, which §5.2 makes the headline. A permanent off-by-one at the centre of every
archived window. Saturating at `0` and `u64::MAX` so the ±1 cannot wrap.

**A capped result is reported as capped, on the wire.** `count` caps outward from the
anchor rather than from the newest end, and §4.2 forbids `complete` for a capped range —
but nothing existing can tell a caller that happened. `LogsExportResult.truncated`
(`rpc_handler.rs:719-733`) is computed only from the resolved lower bound against
`buffer_oldest_seq` and knows nothing about `count`, and comparing requested against
returned cannot distinguish "capped" from "exactly N existed". So: an explicit `capped:
bool`, defaulted, absent-safe.

**Cursor qualifiers (`c>=`) are refused in a capture, and only in a capture.** They commit
the cursor (`rpc_handler.rs:706-717`), so gathering evidence would advance the caller's read
position. The refusal is scoped to the `cases.create` path — extending it to `logs.export`
generally would be a breaking change to a shipped surface, in the same section that claims
its additions are absent-safe. Precedent for the refusal itself: `rpc_handler.rs:611-616`.

### 6.2 `spans.export` — new, because nothing can do this

`.spandata.jsonl` was specified as a deliverable with no mechanism behind it. The span
store's entire read surface is `get_trace`, `slow_spans`, `for_each_matching`,
`recent_traces`, and `context_by_seq` — and `context_by_seq` (`span/store.rs:215-226`)
counts **positions in the span ring** (`idx.saturating_sub(before)`), not seqs. There is no
seq-ranged span read anywhere.

So v1 adds one: `spans.export(from_seq, to_seq, count?)`, walking the span ring and
emitting every span whose seq lies in the range, built on `for_each_matching`'s traversal.

**The range comes from the logs, not from a second window parameter.** §5.1's
`before`/`after` are counts of *log* records; the seq range they resolve to is what scopes
the spans. One `SeqCounter` feeds both stores (`domain.rs:176`), so a single `before = 200`
applied independently to each would yield ~40 logs and 200 spans covering wildly different
wall-clock windows — two files, one verdict, and no way for a reader to know they disagree.
Deriving the span range from the resolved log range makes the two files describe the same
interval by construction.

**The span store has its own ring and its own eviction**, so `.spandata.jsonl` carries its
own line in the Evidence section: what was captured, and whether the ring had already
evicted below `from_seq`. §4.2's verdict speaks for the logs. One verdict cannot honestly
cover two stores with independent retention.

---

## 7. Hazards

| # | Hazard | Handling |
|---|---|---|
| H1 | Registry rots; documents inherit confident-shaped stale provenance | Two timestamps; age never rendered as a verdict (§4.1); `validated_before` on `get`; §3.6.2's coverage line carries per-key ages |
| H2 | A window silently shaped by another session's filter reads as complete | §4.2's **four** verdicts and the epoch log — the correctness property, not a nicety. The disconnected-named-session row of §4.2's table is this hazard's specific form |
| H3 | A lost registry makes re-setting launder old facts as new | **Partly unhandled, and named rather than claimed.** `unknown` carries a cause (§3.3), but `/logmon/first_seen` is written identically for a brand-new domain and for one whose file was lost, so `NeverSet` and `NoRegistry` are not distinguishable from the registry alone. The only real evidence is a quarantine artifact (§3.5), bounded to ten files. Hence the third cause, `Undetermined`, which is the honest default |
| H4 | An fsync under a lock stalls ingest | **Not inherited — re-derived.** §3.5 shows the collector precedent does *not* transfer (nothing on an ingest path reads `domain_data`), and that calling it inherited was the first draft's error. The real hazard is concurrent writers on one domain-keyed file, handled by §3.5's write mutex |
| H5 | Archive grows unbounded | v1 is manual-only, so growth is caller-paced, and the caller supplies `dir` (§5.1) — **the daemon does not know where any archive is**, so it cannot report on one. The draft said `status.get` reports count and bytes; it cannot, and adding archive config is out of §2's scope. `CasesCreateResult` reports this capture's bytes; bounding belongs with watches (§9.1) |
| H6 | A domain name is reused and two eras share a registry, with overlapping seq ranges | `/logmon/incarnation` counts **seq origins** and the document records it beside the seq range (§3.5). A monotone `first_seen` cannot do this and was the draft's error |
| H7 | A case captures a window whose evidence was already evicted | Inherent to a manual path. §4.2 makes it visible, which is the honest answer and part of why watches matter (§9.1) |
| H8 | Two projects on one machine share `default.json` | **Named, not solved.** `config_dir()` is `$HOME/.config/logmon` (`persistence.rs:358-407`) and `DomainId::DEFAULT` is `"default"` (`domain.rs:36`), so every project that does not create its own domain shares one registry — and `default` never re-incarnates, so §3.5's reuse detection cannot see it. §8 therefore tells the agent to create a domain per project, and §3.6.2's coverage line is what surfaces the symptom |
| H9 | A case-folding filesystem merges two domains' registries | `is_valid_name` is case-*sensitive*, so `Build` and `build` are distinct `DomainId`s writing `Build.json` and `build.json` — the same file on APFS and Windows. The registry filename is therefore the domain name **case-folded and suffixed with a short hash of the original**, so distinct domains cannot collide and the mapping stays reversible |

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

**2. When to set them, and the distinction that makes `ttl` honest.** The set is worthless
if it is populated once and rots (§4.1), so the skill names the moments rather than leaving
it to judgement:

- **At session start**, `update_domain_data` with **what you actually re-read** — the commit
  from `git rev-parse`, versions from the lockfile, the profile from the build you just ran.
  Values you re-derived get sent with their values; values you did not, send **key-only** to
  validate. The draft said "restate everything currently believed", which is the same call
  either way and therefore launders the ones you only assumed: §3.2 runs `ttl` from
  `validated_at`, so restating a value you did not check buys it a fresh lifetime it did not
  earn.
- **When the answer changes** — a deploy, a branch switch, a different scenario. `/Action`
  in particular is stale within minutes of switching tasks, and a stale `/Action` is worse
  than an absent one because it reads as fact.
- **Create a domain per project.** H8: everything that does not is sharing one registry with
  every other project on the machine.

**3. How to read the outcomes**, corrected — the draft's version could not happen. It said
`unknown` means the registry was lost under you, but §3.3 makes `unknown` the outcome of a
**key-only** entry, so a call that sends values can never return it. After a lost registry
that call returns `created` for everything, which is indistinguishable from a brand-new
domain, which is H3. So: `created` is news, `validated` is confirmation, `unknown` on a
key-only entry means that key was not there — and the thing to check for a lost registry is
`/logmon/first_seen` against `/logmon/incarnation`, which the draft never mentioned.

**4. Where a fact belongs** (§3.8/§3.9), and it is one rule rather than three: **a `data`
list writes the registry** — `update_domain_data`, `add_collector`, `create_case` are the
same call with different ergonomics. Two exceptions worth a sentence each: *what this run
was built from* goes in the snapshot's `meta`, where it is recorded with the numbers; and
*what is true of this incident but not of the project* — the seed, the iteration, the
hypothesis — takes the `@` sigil on `create_case`, which keeps it in that document.

The rule to teach is the sigil test, in one line: **would this still be true tomorrow, for
the next run? Then it is the registry. Otherwise `@`.**

**5. When to reach for `create_case`**, in the bold "reach for X when…" form the
`profile_traces` fix used — a comparison table alone is what left `profile_traces` unused
in the one production trial we have.

**6. How to read the evidence verdict**, including that `filtered` and `cannot_verify` are
not warnings to skim past: they say the window is not what it appears to be, and a
conclusion drawn from "nothing appeared before the error" is unsound under either.

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

§3.4 gives the reasoning: it is a *filter* over `get`, not a tool. Two drafts cited in-repo
precedent for the cut and **both citations were wrong** — the second one pointed at a
passage about review lenses (*"a fourth reader on the same surface would have been the
weaker instrument"*), where "reader" means a reviewer, not an API consumer. The decision
stands on §3.4's own argument and is now marked as standing on nothing else. Two wrong
citations for one three-line decision is its own signal: the reach for authority was the
tell that the argument felt thin.

### 9.4 The `partial` verdict — cut

The draft defined a fourth verdict for *"the range straddles a trigger post-window, during
which entries were stored unconditionally."* Three problems, the first fatal:

- **Uncomputable.** A post-window is `post_window_remaining: AtomicU32` (`session.rs:90`), a
  per-session countdown decremented per entry (`:738-750`) and zeroed by `set_domain`
  (`:481`). **Nothing records which seqs were inside one.** By capture time it is long since
  zero and the session that held it may be gone. §4.2's epoch log tracks filters, not
  post-windows. The verdict was defined over state the design never added.
- **It would have been the default.** Every session ships `l>=ERROR` and a panic regex with
  a 500/200 window (`engine/trigger.rs:128-158`), so any window containing an ERROR
  straddles one — and case documents are about errors. §8 tells the agent these verdicts are
  *"not warnings to skim past"*; a caveat on every document trains the reader to skip it,
  which then fails on the one where it is real.
- **It points backwards.** A post-window means entries were stored *unconditionally* — more
  completely than normal. Marking that range as degraded inverts the meaning.

v2 can have it behind actual post-window seq recording. `cannot_verify` covers the honest
gap in the meantime.

### 9.5 Cross-checking provenance — cut

The draft compared the registry against a collector's and a case's provenance and warned on
disagreement. **Three of the four comparisons fired on correct usage**, because the design's
own thesis is that these are different facts at different moments (§3.8). Registry-vs-collector
fires on every deploy; arm-vs-snapshot fires on the flagship A/B gesture, because `reset`
defaults to `true` and the window is swapped at each snapshot. The fourth — comparing two
snapshots' broker versions — is sound in principle and blind in practice, since no snapshot
already on disk carries one and a key present at one end only is not a mismatch.

What replaces it is in §3.8: render the ordering. **A warning is a claim that something is
wrong, and may only be emitted where wrongness is what was detected.** That rule is the
most portable thing this gate produced.

---

## 10. Test list

**Verification:** both timestamps round-trip through a restart; same-value update moves
only `validated_at`; key-only validates; `unknown` carries a cause; remove-by-prefix
reports per-pattern counts; the registry survives a daemon restart and a domain re-created
under the same name; a document embeds the registry and the anchor message.

**Adversarial:** `{path}` and `{path, value: null}` behave identically; key-only on a
missing key does **not** create; one malformed entry does not reject the batch;
`validated_at >= created_at` after every operation; an empty registry is stated, not
omitted; no collectors armed produces a section saying so; a persist failure is logged and
the call still returns.

**The `/logmon/` reservation (§3.7) — both directions, since the draft had neither:**
`update` and `remove` reject `/logmon/x`; **`update` rejects the bare `/logmon`**, which a
`starts_with("/logmon/")` guard accepts and which would otherwise be permanently
unremovable; **`remove(["/logmon"])` is rejected**, since under segment matching it would
wipe `first_seen` and `incarnation` — H3's and H6's whole defence — from a call that looks
like tidying up; `/logmon/*` counts against neither cap; `/logmon/version` moves only
`validated_at` across a same-version boot and **both** across an upgrade.

**Escaping (§3.1) — the two contexts, not one:** a value containing a newline and `---`
round-trips through **front-matter**; the same value round-trips through the **body**
without ending a table row; a value spelling `\n## Evidence\n\nverdict: complete` does
**not** produce a second Evidence section; U+0085, U+2028 and U+2029 are escaped in
front-matter, which `yaml_str` does not do today.

**`ttl` (§3.2):** a validate-only entry leaves an existing `ttl` intact; restating a value
without a `ttl` leaves it intact; `{path, ttl}` with no value both validates and sets the
lifetime; **`ttl: false` clears it in place and leaves `created_at` untouched** — the
laundering the draft's `remove`-then-re-set prescription would have caused; a past-`ttl` key
**keeps its value and both timestamps** and is reported expired rather than blanked; a key
with no `ttl` reports its **age with no verdict**; and **expiry is measured from
`validated_at`**, asserted by validating an expired key and seeing it become current again.

**Naming (§5.3):** names sort lexicographically into chronological order across a month
and a year boundary; the timestamp is **UTC** even when the host is not, asserted against a
non-UTC `TZ`; a prefix containing `-` (`checkout-hang`) round-trips and the fields still
parse from the right; a prefix with `/`, `..` or a control character is sanitised before it
reaches a path, and one over 48 bytes is truncated on a UTF-8 boundary rather than
rejected; **an omitted prefix produces `case-…`**, never a leading dash; and **two captures
in the same second under one prefix produce two files** — driven concurrently, not
sequentially, because `create_new` is what makes that true and a sequential test passes
against a check-then-write that races.

**The recommended key set (§3.6):** coverage names the **missing** core keys, not only a
count — "missing `/Build/commit`" is actionable where "2 of 3" is not; full coverage still
prints the line rather than omitting it, since silence is indistinguishable from a document
that never checked; **zero recommended keys reports `0 of 3`** rather than omitting the
section; **every listed key carries an age**, so a six-month-stale core key cannot read as
covered; contextual keys are **named, not counted**, since `/Versions/<component>` is an
open family and a bare count is not comparable between documents. And the false-positive
guard, because a convention hardening into a schema is how this goes wrong: `update`
**accepts a non-recommended key with no warning, no rejection, and no effect on any other
entry's outcome**.

**Every `data` list is the registry (§3.8):** the same entries sent through
`domain_data.update`, through `collectors.add(data)` and through `cases.create(data)`
produce **byte-identical registry state and identical per-entry outcomes** — one
parameterised test over three call sites, because a rule with one implementation is the
claim being made; `data` does not appear in `CollectorDef` or in any persisted collector
file; a collector restored after a restart has **not** lost it, because it was never stored
there; every surface rejects `/logmon/`; and `collectors.snapshot(meta)` is unchanged — a
`meta` key `logmon` is stored verbatim as it is today, since nothing folds it into a path
any more.

**The `@` sigil (§3.9):** `@/Data/seed` on `cases.create` appears in the document and the
registry is **byte-identical afterwards**, asserted by reading it back; the sigil **survives
into the rendering**, so a reader can tell an assertion from domain state without a legend;
`@/logmon/version` is **rejected**, because a sigil does not launder the reservation; a
sigil key on `domain_data.update` or `collectors.add` is **rejected with a message naming
why**, not silently dropped — neither has a document to put it in; `@/x` and `/x` may both
be present and are **two different keys** that do not overwrite each other; and sigil keys
count against their own cap, not the registry's 256.

**No cross-checks (§9.5) — the mutation to run:** delete the ordering renderer and confirm a
test goes red. A document whose registry `/Build/commit` came into force **after** a
snapshot's `taken_at` renders both instants; it emits **no warning**, asserted explicitly,
because the draft's version of this fired on every deploy.

**The evidence verdict (§4.2):** a capture taken while another session holds a filter
reports `filtered`, **names the filter and the seqs**, and does not report `complete`; an
**empty store reports `cannot_verify`, not `complete`** — the draft's normative table
admitted this and only its prose forbade it; a range **straddling a daemon restart** reports
`cannot_verify`; a **disconnected named session's filters still narrow**, so a marker keyed
on connected sessions produces a false `complete` (H2's exact form); `filters.edit` opens a
new epoch, so a document never attributes the new filter string to the old range; a range
that is **capped** is never `complete`, driven by the explicit `capped` flag rather than by
comparing requested against returned; and **`partial` does not exist** — asserted, because
a removed verdict that a renderer still emits is the failure mode.

**Range reads (§6):** `from_seq`/`to_seq` are **inclusive**, asserted with `before = 0`,
`after = 0`, which must return **exactly the anchor entry** — the off-by-one that a strict
lowering would put at the centre of every archived window; `from_seq = 0` and
`to_seq = u64::MAX` do not wrap; a range starting below `buffer_oldest_seq` reports
`evicted`; `c>=` is refused on the capture path and **still accepted on plain
`logs.export`**, since scoping the refusal wrongly would break a shipped surface; and
`spans.export` over the resolved range returns spans whose seqs lie in it, with the span
ring's own eviction reported separately from the log verdict.

**`cases.create` (§5.1):** a relative `dir` is **rejected**, not resolved against the
daemon's cwd; the returned `paths` are what actually exist on disk; `verdict` on the wire
equals the verdict in the document; `notes` entries carry a `kind`; an **unresolvable
anchor is an error**, not a document with an empty headline; a `trace_id` matching many
entries anchors on the earliest by seq and says so; and unsigilled `data` is applied to the
registry **before** the registry copy is rendered, so a key the capturer supplies appears in
that document rather than landing one document late.

---

## 11. A worked example

Neither contract this document mints was ever exhibited in the drafts — no front-matter, no
registry file, no call. For a T2 whose stated reason for being T2 is minting a read-forever
format, that was the largest single gap the gate found, and it costs three blocks to close.

**The registry file** — `~/.config/logmon/domain_data/<folded-name>-<hash>.json`:

```json
{
  "format_version": 1,
  "entries": {
    "/Build/commit":  {"value": "9f2a1c4", "created_at": "2026-07-31T08:12:04Z",
                       "validated_at": "2026-07-31T14:03:11Z"},
    "/Action":        {"value": "checkout smoke, 20 iterations",
                       "created_at": "2026-07-31T13:58:00Z",
                       "validated_at": "2026-07-31T13:58:00Z", "ttl": "30m"},
    "/logmon/version": {"value": "0.9.0", "created_at": "2026-07-28T09:00:00Z",
                        "validated_at": "2026-07-31T09:14:22Z"}
  }
}
```

**One lifecycle**, with the outcomes that come back:

```
update_domain_data([                      → /Build/commit  created
  {path: "/Build/commit", value: "9f2a1c4"},  /Action       created
  {path: "/Action", value: "checkout smoke, 20 iterations", ttl: "30m"},
  {path: "/Env/host"}                         /Env/host     unknown{undetermined}
])
add_collector(name: "checkout", filter: "...",   → same registry, same outcomes
              data: [{path: "/Build/profile", value: "release"}])
snapshot_collector(name: "checkout", label: "before",
                   meta: {"git_sha": "9f2a1c4"})   ← run provenance, not the registry
create_case(reason: "hang at 20/20", anchor: {seq: 41022},
            dir: "docs/cases", prefix: "checkout-hang",
            data: [{path: "/Env/host", value: "ci-7"},      ← registry: true of the domain
                   {path: "@/Data/seed", value: "8814"}])   ← document only: about this case
```

**The document's front-matter** — fixed schema, `grep`-able, core keys only:

```yaml
---
case: checkout-hang-260731-021530
captured_at: "2026-07-31T14:15:30.288Z"
domain: t3
incarnation: 2
reason: "hang at 20/20"
anchor: {kind: seq, seq: 41022, at: "2026-07-31T14:02:07.113Z"}
headline: "checkout worker stalled awaiting lock"
verdict: filtered
seq_range: {from: 40672, to: 41372, capped: false}
logdata: {file: "checkout-hang-260731-021530.logdata.jsonl", records: 700}
spandata: {file: "checkout-hang-260731-021530.spandata.jsonl", records: 168}
provenance: {core: "2 of 3", missing: ["/Build/profile"]}
asserted: {"@/Data/seed": "8814"}
---
```

`/Env/host` went to the registry and is rendered in Provenance with everything else, with
its age. `@/Data/seed` did not, and appears here under `asserted` — the sigil kept in the
key, so a reader who greps the archive for `/Data/seed` and one who greps for `@/Data/seed`
are asking two different questions and get two different answers.

`anchor.at` and `captured_at` differ by thirteen minutes, and both are present, because
§5.1 argues that a case is about the incident rather than about now — and because the
registry copy is as-of `captured_at`, so every age in the document is measured from there
and not from the moment being investigated.

**The Evidence section**, which §5.2 puts before anything it qualifies:

> ## 2. Evidence
>
> **`filtered`** — session `web-agent` held `service:checkout` over seqs 40672–41100, so
> 428 of this window's 700 seqs were stored matches-only. "Nothing appeared before the
> stall" is not a supported conclusion over that range.
>
> Spans: 168 captured; the span ring had not evicted below seq 40672.
>
> Provenance: **2 of 3 core keys** — missing `/Build/profile`. Present: `/Build/commit`
> (6h), `/Action` (17m, within its stated 30m lifetime). Contextual: `/Env/host` (0m).
> Asserted for this case, not validated: `@/Data/seed` — recorded at capture, 13m after
> the anchor.
>
> Collector `checkout` — snapshot `before` taken 4h ago, `git_sha: 9f2a1c4`;
> `/Build/commit` has been in force since 6h ago.

That last line is §3.8's ordering: two instants, no judgement, and a reader who can see for
themselves that the snapshot is not from a different build.

---

## 12. What the second gate changed

Four lenses over the frozen `d5a3199` returned 58 findings. The **format** decisions all
survived — JSONL, no compression, the log/span split, UTC and parse-from-the-right, the two
timestamps, segment-boundary matching — several cleared independently by two or three
lenses. The **mechanism** decisions largely did not.

| Was | Is | Because |
|---|---|---|
| Three key/value namespaces: registry, `/collector/data/`, `/case/data/` | **One registry.** `data` on `collectors.add` and `cases.create` is `domain_data.update`; `@` scopes a key to one document (§3.9) | Ten findings existed only because the subtrees were a *second* mechanism beside the registry. `collectors.snapshot(meta)` was already the answer to "what was this run built from" |
| Four cross-checks warning on provenance disagreement | **None.** Render the ordering instead (§3.8) | Three of four fired on correct usage — registry-vs-collector on every deploy, arm-vs-snapshot on the flagship A/B gesture. §9.5 |
| A `meta` fold table, two aliases, a merge rule | **`meta` untouched** | The fold produced `/collector/data/build_profile` while the registry produced `/Build/profile` — so the flagship check could never fire on the two keys the whole design was about |
| `/logmon/version` stamped per collector, anchored to window start | Cut | `registry.rs:379-381` restores with the original `armed_at` and never sets `zeroed_at`, so `window_start()` is pre-restart. The fix for the first draft's false positive was a no-op, and the correct anchor made the check unable to fire at all |
| Verdicts `complete`/`evicted`/`filtered`/`partial` | `complete`/`evicted`/`filtered`/**`cannot_verify`** | `partial` was uncomputable and would have appeared on nearly every document (§9.4). `cannot_verify` was prose the normative table contradicted |
| Three narrowing-marker flip sources, called closed | **Nine**, including daemon restart and disconnected named sessions | A closed-world claim that is short is not a gap, it is a false `complete` (§4.2) |
| Client writes; daemon computes the stem | **Daemon writes, `dir` required** (§5.1) | Neither party could resolve a collision. Two agents capturing in one second would have overwritten each other, which §10 forbids |
| `ttl` cannot be cleared; anchor unstated | **`ttl: false`; anchored on `validated_at`** (§3.2) | The prescribed `remove`-then-re-set was the laundering §3.3 forbids, and the repo had already shipped the shape of the answer at `methods.rs:1068` |
| `logs.export` range only | **plus `spans.export`** (§6.2) | `.spandata.jsonl` had no read path; `context_by_seq` counts ring positions, not seqs |
| Caps on stored keys | **plus a rendered-registry cap and `document_bytes`** (§5.2) | §10 declared legal a document with ~1.9 MB of provenance over ~0.4 MB of evidence |
| No worked example | **§11** | Neither contract this spec mints was ever exhibited, in a T2 whose stated reason is minting a read-forever format |
| No format version in the archive | **A header record per JSONL file** (§5.2) | §2 makes the format the contract, and the collector path already had `FORMAT_VERSION` for the same reason |
| "Per-entry outcomes", untyped | **Enums** (§3.3), incl. a third `unknown` cause | `rejected` reasons were never enumerated, and the two stated causes are not distinguishable from the registry alone |
| No module, no phases | **§2** | Four files change before any feature logic exists; "Phase 1 is unblocked" named no phase |

Two citations were wrong and are corrected in place: `server.rs:530` is `export_logs`
writing to a caller-supplied path (it was cited as evidence that clients write — it is the
counter-example), and §9.3's precedent was a passage about *review lenses*. One `safe_name`
was described two incompatible ways in one document. §1's header claimed every row named a
constructor and a caller while four did not, which is why §1 now has a `Kind` column.

**The rule worth carrying out of this gate:** a warning is a claim that something is wrong,
and may only be emitted where wrongness is what was detected. Three of four checks here
warned about differences the design existed to make legitimate.
