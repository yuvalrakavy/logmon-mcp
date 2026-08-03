# Case loading and production forensics — proposal

**Status: PROPOSAL, not a design.** This is the input to a design pass, captured so it
is findable later. Nothing here is settled except the section marked *Settled*. No
shape has been chosen and no option table has been built.

**Sequencing (user-directed 2026-08-03):** log aggregation is redesigned and
implemented first. This is designed properly only after that lands.

**Keywords for whoever greps before designing:** case load, load_case, postmortem
domain, archive replay, production forensics, offline investigation, case transfer.

> Filed this way deliberately. The log-aggregation gate found that feature had already
> been proposed as **B4** in `2026-06-30-domains-and-broker-improvements-proposal.md`
> and deferred by name — and the new spec never cited it, because nobody grepped the
> proposals before designing. This document exists to be found by that grep.

---

## 1. The idea (user, 2026-08-03)

Case documents already capture the state of logs and spans as files on disk. Make them
**loadable back into a domain** — a "postmortem" domain — so the whole existing read
surface can query them.

> "The AI will be able to load a case file, figure out what was the slowest span 3
> months ago and compare it to the current value."

The payoff lands on tools that already work retrospectively: `profile_traces` and
`get_slow_spans` over the loaded domain answer the historical half, and the same calls
without `--domain` answer the current half. Collectors, being forward-facing, are
useless here — which is itself a design constraint, see §6.

## 2. The scenario that raises the stakes (user, 2026-08-03)

> "A logmon installed on a production machine with case file creation filters. Those
> case files are then transferred to a development machine where engineers with AI
> tools can investigate and get insights on the production system."

This is a bigger claim than offline replay of your own captures, and it changes the
weight of several open questions:

- **Provenance stops being optional.** On the dev machine nobody knows what the
  production system *was*. The `data` parameter on `create_case` and the `domain_data`
  registry already exist for exactly this — commit, build profile, scenario — and an
  `@`-prefixed key is asserted about the capture alone. A loaded case that does not
  carry its provenance is an artifact nobody can reason from.
- **The archive becomes an interchange format between machines**, not just across
  time. `FORMAT_VERSION` was justified as "the contract a reader has years later"; here
  it is also the contract between two installs that may be at different versions.
- **Privacy and secrecy become real.** Production logs may carry PII, tokens, or
  customer data, and this scenario moves them onto developer laptops. Whatever the
  answer is — redaction filters at capture time, an explicit acknowledgement, nothing
  at all — it should be a decision recorded in the design rather than an omission.
- **Volume.** `create_case` caps `before`/`after` at 5000 records each. Whether that
  is the right ceiling for an unattended production capture is an open question.

## 3. Why this does not conflict with the case-documents non-goal

`2026-07-31-case-documents-design.md:101` says:

> **Out permanently:** any query engine over the archive. The format is the contract;
> indexing belongs to whatever walks the directory.

**Read precisely, this proposal respects that rather than reversing it.** What is ruled
out is logmon indexing and querying the files *in place*. Loading hands records to the
ordinary in-memory store and queries them with the engine that already exists: the
archive stays a dumb file, nothing indexes it, and the loader *is* the "whatever walks
the directory" that sentence delegates to.

**Confirmed by the user, 2026-08-03:** *"case file management and query is outside the
scope of logmon."* The non-goal stands as written, and this proposal sits inside it
rather than against it.

### The two-layer split that follows

Confirming the non-goal settles the boundary, and it is worth stating as a rule because
it decides where future requests land:

| Question | Answered by |
|---|---|
| *Which case do I want?* — search across captures, by time, service, symptom, tag; retention; archival | **An external management layer.** The user names a document database such as MongoDB as the natural fit: the evidence is already JSONL, one record per line, and the document carries front-matter |
| *What happened inside this case?* — the slowest span, the error distribution, the timeline | **logmon**, by loading that one case into a postmortem domain |

So the loader's contract is deliberately minimal: **it takes a path to a case file.**
How that file arrived — `scp` from production, materialised out of MongoDB by a
management tool, pulled from an artifact store — is not logmon's business, and the
design should resist any pull to make it so.

This is also why `FORMAT_VERSION` and the format-as-contract line carry so much weight.
With management living outside, **the file format is the entire interface** between
logmon and whatever curates the archive. It is the one thing that cannot be changed
casually.

## 4. What already exists (grounded 2026-08-03)

| Enabler | Evidence |
|---|---|
| Case files are markdown + `<stem>.logdata.jsonl` + `<stem>.spandata.jsonl` | `crates/core/src/cases/naming.rs:98–99` |
| Evidence is **full-fidelity** — `write_jsonl` serializes the store's records, not a projection | `crates/core/src/daemon/rpc_handler.rs:1868` passes `logs.iter()` through a generic `write_jsonl<T: Serialize>` |
| `LogEntry` derives `Deserialize`, so the round trip is plausible | `crates/core/src/gelf/message.rs:91` |
| Every JSONL carries `FORMAT_VERSION` as its first line, and the document front-matter repeats it | `crates/core/src/cases/mod.rs:43` |
| **A receiverless domain is already possible** — `domains create` documents that "a port of 0 disables that receiver" | `domains create --help` |
| Domains are already isolated: own buffers, receivers, triggers, filters | `domains.create` / `use_domain` |
| Provenance capture already exists and rides along with a case | `create_case(data:…)`, `domain_data.*` |

## 5. Settled (user decisions, 2026-08-03)

- **A postmortem domain is not bound and cannot receive new data.** Sealed, not merely
  unlistening — nothing arrives by any path.
- **Operations that would be inert on a sealed domain yield errors**, rather than being
  accepted and silently doing nothing.

Consequences that follow and should carry into the design:

- **Creation and loading are one atomic operation.** An empty postmortem domain that
  can never receive data is a useless object, so `create` + `populate` is one call.
- **Buffer sizing stops being a hazard.** Size to the file's record count at load; with
  no further ingest, nothing ever evicts, so the loaded case *is* the case until the
  domain is deleted.
- **`seq` can be preserved**, since nothing else will ever write to that domain — which
  keeps the case document's `@seq` cross-references pointing at the right records. This
  holds only for **one case per domain**; loading a second into an occupied postmortem
  domain must be refused or the collision returns.

### The refusal surface

Each refusal should name the reason, and where possible the alternative:

| Operation | Why it must error rather than be accepted |
|---|---|
| `add_collector` | Collectors are forward-facing. Armed here it reports zero forever, which reads as *"I measured and nothing happened"* rather than *"I could not measure."* The refusal should point at `profile_traces`, which projects over spans already in the buffer |
| `add_trigger` | Same shape, quieter: an armed trigger that cannot fire looks like an all-clear |
| `add_filter` | Filters shape what gets *stored* at ingest; with no ingest they do nothing, while the user believes the domain was narrowed |
| `clear_logs` / `clear_domain` | The live buffer refills; this one cannot. Deleting the domain is the honest way to discard it |

Bookmarks and cursors stay available — read-side navigation is how a reader moves
between the document's narrative and its evidence.

## 6. What does NOT exist, and is the largest piece of new work

**Unattended case writing.** The production scenario needs a case file to appear on
disk with nobody connected. Today:

- `create_case` is reachable only as a client RPC call (`rpc_handler.rs:1868–1877`).
- A trigger captures a window and **notifies a session**; it does not write files.
  Fires queue for a disconnected named session and replay on reconnect — they do not
  become artifacts.

So "case file creation filters" on a production box is a genuinely new capability: a
**daemon-side autonomous action**, which is a different class from everything the
daemon does today, since every existing side effect is client-initiated. It brings its
own questions — where files land, rotation and retention, disk budget, what happens
when the disk is full, and whether an unattended capture is allowed to write at all
without an operator having configured a directory.

**This may deserve to be its own design**, separate from loading. Loading is useful
without it (engineers capture cases by hand today); unattended capture is useful
without loading (files can be read by a human). They compose but do not depend on each
other, and fusing them would produce one large design instead of two small ones.

## 7. Open questions for the design

1. **Does loading go through the ingest pipeline, or straight into the buffers?**
   Largely settled by §5 — a sealed domain has no ingest — but it forecloses replay,
   see §8.
2. **`seq` preserved or reassigned?** §5 leans preserved; the design must state it and
   handle the two-cases-one-domain case.
3. **`FORMAT_VERSION` mismatch** must refuse loudly rather than parse what it can. Note
   the direction that matters for §2: a *newer* file on an *older* dev daemon.
4. **Does the domain announce itself as postmortem in `get_status`?** Probably must —
   every staleness, idle and liveness figure will otherwise read as alarming on
   months-old data.
5. **Is `create_case` *from* a postmortem domain allowed?** Harmless re-export, or
   confusing provenance?
6. **Where does the loader live?** `load_case` is the inverse of `create_case`, and
   inverse pairs want one owner — an argument for `crates/core/src/cases/`, so the
   format contract has a single home rather than a reader and a writer that can drift.
7. **Does the loader accept anything but a filesystem path?** §3 says no by default —
   a path is the whole contract, and a management layer materialises a file first. The
   design should confirm that rather than inherit it, since the alternative (accepting
   a stream, or a URL) is the first step toward logmon knowing about the archive.

## 8. Deliberately foreclosed, and what it costs

Sealing rules out replaying a case through the pipeline, which means **"would this
trigger have caught it?"** cannot be answered against a case file. Validating an alert
against a known incident is legitimately valuable, and this decision forecloses it.

That is judged the right trade: replay is a different feature with different semantics
— it needs a clock, an ordering, and a notion of "as if live" — and bolting it onto a
postmortem domain would leave the domain neither sealed nor live. Recorded here so that
whoever wants it later builds **replay**, rather than un-sealing this.

## 9. Related work

- `2026-07-31-case-documents-design.md` — the writer side, and the §2 non-goal this
  proposal must be reconciled against.
- `2026-06-30-domains-and-broker-improvements-proposal.md` — the sibling proposal
  document; **B4** there is a live, still-unbuilt item.
- Log aggregation (`2026-08-02-log-aggregation-design.md`) — precedes this in the
  queue, and its `profile_logs` would apply to a loaded domain for free.
