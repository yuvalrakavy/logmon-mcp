# Daemon-taught tools — design

**Status:** **WITHDRAWN at the design gate, 2026-07-30.** Superseded for its
immediate purpose by `2026-07-30-capability-skew-visibility-design.md`, which
shipped in 0.9.0. Kept for its reasoning, and because §9's redesign is live.

**Why it was withdrawn** — two objections from the gate (~35 findings), recorded
in `9706a23`'s commit message and in `docs/process/retro-log.md`:

1. **It deletes argument validation across all 42 tools.** `ToolRoute::new_dyn`
   takes a raw `JsonObject` and **rmcp validates nothing against `inputSchema`**,
   so a dynamic route receives whatever the client sent. The compiled-in tools it
   would have replaced at least get a serde type check.
2. **It cannot deliver its own bootstrap explanation.** Concretely: `main.rs`
   `?`-exits before `serve()`, so a failed manifest fetch kills the process before
   the MCP server starts. §4.1's "expose zero tools plus `instructions`" cannot
   run at all on that path.

**Why this line exists.** For a day this file's status read *"draft for design
gate. No code written"*, which any reader takes to mean *pending*. It was not
pending; it was decided.

The honest failure is narrower than "nobody wrote it down", because two places
did. It is that **the decision was not where the decision's own artifact was**,
and a reader who opens the spec — as the author did on 2026-07-31 — finds a status
line that contradicts it. The cost was real: it was re-proposed as new work, and
the re-proposal was accepted before either record was opened.

Two guards, then, not one. **Record a withdrawal in the artifact it withdraws.**
And, for the reader: *a spec whose status reads "pending" is a claim like any
other* — check the retro log and `git log --` the file before believing it.

**→ §11 IS THE PLAN. Start there.** Everything above it is how it was arrived at.

§10 (revision 3) was gated by four lenses on `7a61856`. **§10.2 — generating the CLI — did
not survive** and is dead (§11.5). The rest of §10 stands, and §11 sequences it behind a
Phase 0 that may make the whole thing unnecessary — which is the point of putting Phase 0
first.

**Read in this order:** §11 for the plan, §10.1/§10.3 for the manifest shape it uses,
§9.3–§9.6 for what that inherits, §9.8 for what is dead in §§0–8, and §§0–8 only for
reasoning. **"Current" is still not "approved to build"** — §11 has been through an
architect/reviewer pass (§11.6) but not a gate.

**Tier:** T2 — mints a wire contract (the descriptor manifest) that outlives the code.

**Seams verified at:** `98e75cc` for §§0–8 — **and several are now known false**, see §9's
header and §9.8. §9 revision 2's claims are grounded at `dfa37a5`.

---

## 0. Motivation

On 2026-07-30 the Store project used the collectors for the first time and filed
a report. Three of its seven suggestions — labelled arms, compare-two-arms, and
carry-provenance — described features that had **already shipped**
(`snapshot_collector(label=)`, `collectors.diff`, snapshot `meta`). They did not
know, because their MCP shim binary was three minor versions behind the daemon
and the tool list is compiled into the shim. Their own conclusion:

> the binding constraint right now looks like documentation rather than capability

The narrower reading is that they needed better docs. The load-bearing reading is
that **a shim upgrade is a precondition for a daemon capability being usable at
all**, and nothing tells anyone when that precondition is unmet. A version
handshake would report the skew; this design removes the skew.

The goal: **the daemon teaches the shim what tools exist.** Upgrading the daemon
makes new capabilities reachable without rebuilding or restarting the shim.

---

## 1. Seams — verified at `98e75cc`

| Seam | Status |
|---|---|
| Runtime tool registration | **Confirmed.** `ToolRoute::new_dyn(attr, call)` (rmcp `router/tool.rs:177`) takes a closure handler; `.parameters_value(serde_json::Value)` (`:286`) takes the input schema as a runtime value rather than a compile-time `JsonSchema` type. `ToolRouter::add_route` (`:383`), `merge` (`:389`), `remove_route` (`:395`), `list_all` (`:415`). Nothing here needs to fight the framework. |
| Telling the client the list changed | **Confirmed.** `ServerCapabilities::enable_tool_list_changed()` (`model/capabilities.rs:439`); `ToolListChangedNotification` = `notifications/tools/list_changed` (`model.rs:1383`), present in the `ServerNotification` enum (`model.rs:3235`). |
| Telling the *model* something, with no tool call | **Confirmed.** `InitializeResult.instructions: Option<String>` (`model.rs:832`), `with_instructions` (`:847`). This is the channel for "the daemon is down", and it requires no tool to exist. |
| Reconnect | **Confirmed, with a precondition.** `crates/sdk/src/reconnect.rs` already implements `Connected → Reconnecting (exponential backoff) → PermanentlyFailed`. **But** an anonymous session goes straight to `PermanentlyFailed(SessionLost)` on any disconnect (`reconnect.rs:19-21`), and the shim names its session only conditionally (`crates/mcp/src/main.rs:106-108`). The shim must always name itself. |
| A named session surviving a daemon restart | **Confirmed.** Named sessions are persisted (`daemon/persistence.rs`, `named_sessions`), so a restarted daemon returns `is_new: false` and reconnect completes rather than terminating (`reconnect.rs:179-186`). |
| Method↔tool mapping | **Confirmed 1:1, counts stale.** At `dfa37a5` it is **45 and 45**, not 43 and 42. But *"nearly mechanical, not a curation problem"* is the load-bearing half and it is **wrong for a different reason** than the count: the mapping is 1:1 by *name* while the **parameter sets differ in 30 of 45 pairs** (§9.1). |
| Tool bodies | ~~**Confirmed.** 42 tools, 42 `.call(` sites — exactly one RPC per tool, no orchestration anywhere. 41 return `to_string_pretty(&result)` verbatim.~~ **FALSE, and false when written.** There are three non-passthrough tools, not one: `export_logs` formats locally and `std::fs::write`s a caller-supplied path (`server.rs:519-573`), `get_status` mutates the payload to insert `shim_note` (`:456`), and `document_collectors`. The count was reached by counting `.call(` sites, which cannot distinguish a passthrough from a body that also writes a file. See §9.5. |
| The one exception | **Deliberate, but not the only one** (see "Tool bodies"). `document_collectors` (`server.rs:1078`, cited as `:1045`) returns the markdown itself and appends `warnings` / `sidecar_name` as blockquotes — because "a reader that has to unwrap a string field before it can read the markdown will spend a turn doing that, and the warnings are what it would skip". Note it names `sidecar_content` in that blockquote and **does not return it**. |
| Param structs | ~~**Confirmed.** 34 `*Params` structs in `mcp/src/server.rs`; **zero** in the protocol crate. This is a relocation, not a de-duplication.~~ **FALSE.** `crates/protocol/src/methods.rs` has **128 `JsonSchema` derives** and a committed `protocol-v1.schema.json` regenerated by `cargo xtask verify-schema`. It is a **de-duplication of three divergent sources**, and 30 of 45 pairs have already drifted. See §9.1 and §9.2. |
| Discovery RPC | **Confirmed absent.** No `rpc.discover`, no `list_methods`. The daemon currently has no way to say what it can do. |
| CLI shape | **Confirmed.** 2689 lines across 7 files. Every command already supports `--json`. Arrays are repeatable singular flags (`--group-key`); the one nested object is prefix-flattened (`--threshold-metric/-op/-value`); `meta` is the sole raw-JSON parameter (`cli/collectors.rs:106`). |
| CLI enum validation | **Confirmed absent.** `--level tre` passes clap and fails at the daemon: `rpc error -32601: unknown level 'tre'`. Enums live as prose in help text, so clap cannot enforce them. |

### 1.1 What this design does NOT claim

- It does **not** remove the need for a version exchange. It narrows its job to
  negotiating the *manifest format* (§3.5).
- It does **not** make the shim stateless. It still owns transport, the MCP
  handshake, and the human-rendering overlay in the CLI (§5).

---

## 2. Scope

**In:** all 42 MCP tools become daemon-taught, in one cutover. The CLI's argument
parsing and dispatch are generated from the same manifest.

**Out:** the CLI's human-readable renderers (they degrade to `--json`, §5.3).
Remote or multi-tenant operation — see the hard constraint in §6.1.

---

## 3. The descriptor manifest (the contract)

### 3.1 New RPC: `tools.manifest`

Takes no parameters. Returns:

```json
{
  "format_version": 1,
  "daemon_version": "0.9.0",
  "tools": [ { ...descriptor... } ]
}
```

### 3.2 One descriptor — an MCP `Tool`, plus routing

The descriptor is **the MCP `Tool` shape as specified**, with logmon-specific
fields under a `x-logmon` key that the shim strips before forwarding:

```json
{
  "name": "get_collector",
  "description": "Read a collector's numbers. …",
  "inputSchema": { "...": "JSON Schema, draft 2020-12" },
  "annotations": { "readOnlyHint": true, "destructiveHint": false },
  "x-logmon": { "method": "collectors.get", "result": "json" }
}
```

Not a bespoke shape, for three reasons. The shim forwards these to the client
**unchanged** rather than translating between two vocabularies — translation is
where fields get dropped. The field names are then fixed by a published spec
rather than by us. And the CLI generator (§5) consumes a standard shape, so it is
retargetable at any MCP server rather than being logmon-only (§5.5).

| Field | Why it exists |
|---|---|
| `name` | The MCP tool name. Deliberately **not** derived from the method: `collectors.get` → `get_collector` is a naming convention the agent-facing surface owns. |
| `description` | **The product.** §0's finding is that descriptions decide whether a capability is used. Moving them here is what makes a new tool usable by an un-upgraded shim. |
| `inputSchema` | Fed to `ToolRoute::parameters_value` verbatim, and to the CLI generator (§5). |
| `annotations` | MCP's own `readOnlyHint` / `destructiveHint`. Cheap now, expensive to add later. |
| `x-logmon.method` | The RPC the shim forwards to — the whole body of a generic handler. |
| `x-logmon.result` | `json` or `text` — see §3.4. |

### 3.3 Schema requirements the generator depends on

The `params` schema is not merely descriptive; two properties are load-bearing
for §5, and if the daemon omits them the CLI silently degrades to raw JSON:

- **Enum constraints must be `enum`, not prose.** Today `--level`'s help says
  "scalar | timing | tree" in English and clap cannot enforce it (§1). As data,
  it becomes `possible_values`: local validation, an error listing the valid
  values, and value completion.
- **A free-form object must be distinguishable from a structured one.**
  `meta` accepts arbitrary JSON; `threshold` has known fields. The first
  generates `--meta k=v` (repeatable), the second generates prefixed flags. With
  no way to tell them apart, both fall back to raw JSON.

### 3.4 `result: text`

41 of 42 tools return the daemon's JSON verbatim, so a generic handler covers
them. `document_collectors` returns markdown directly, with warnings appended as
blockquotes, for a reason §1 records and this design must not undo.

**Resolution:** the *daemon* composes the final text — appending its own warning
and sidecar notes — and the descriptor marks `result: text`. The shim then emits
it as text rather than pretty-printed JSON. This makes the shim **42 of 42**
passthrough and moves the composition next to the data it describes.

### 3.5 Versioning the manifest itself

`format_version` is the manifest's own contract, independent of
`PROTOCOL_VERSION` (which stays 1; adding a method is additive).

- The shim **ignores unknown descriptor fields**, so the daemon may add fields
  freely — the same additive discipline the persisted format already follows.
- The shim **refuses a `format_version` greater than it understands**, reports
  why via `instructions`, and serves no tools. A newer manifest may rely on
  semantics this shim lacks; guessing is how a tool gets misdescribed to an agent.

### 3.6 A descriptor the shim cannot build a route from

**Skip the one tool, keep the rest, and name it in `instructions`.** Refusing the
whole manifest because of one bad row is the "one problem blanks the entire
response" failure fixed in 0.8.0 (`excluded_by_warmup`, `groups_total`) — the
same shape, one layer up.

---

## 4. Lifecycle

### 4.1 Bootstrap: daemon unreachable

**Expose zero tools.** Not a cached manifest, not a hardcoded diagnostic core.

The rationale is structural rather than stylistic: with no tools registered it is
**impossible** for the agent to call a tool that cannot work. A cached manifest
advertises tools that will fail, and can advertise tools a newer daemon has since
removed. A hardcoded core routes "the daemon is down" through a tool call the
agent has to think to make.

The silence is explained in `instructions` (§1), which reaches the model with no
tool call:

> `logmon-broker` is not running at `<socket path>`. No tools are available.
> Start it with `<command>`. Tools will appear automatically once it is running.

**Why not fail `initialize`:** clients call it once. A hard failure marks the
server dead until a manual reload, so a daemon that starts thirty seconds later
is not picked up — strictly worse than today, where the shim auto-starts the
broker (`mcp/src/auto_start.rs:46`).

### 4.2 The daemon appears, or changes

1. The SDK reconnects (already built; requires §1's named-session precondition).
2. The shim calls `tools.manifest`.
3. It diffs against the registered set: `add_route` for new, `remove_route` for
   gone, replace for changed.
4. If anything changed, it sends `notifications/tools/list_changed`.
5. If anything was **added**, it arms the banner (§4.3).

### 4.3 Telling the *agent*, not just the client

`list_changed` updates the client's tool table. The agent is told nothing, and
tools that appear silently are indistinguishable from tools that were never
there — **the §0 failure, one level up.** A separate mechanism is required.

**A one-time banner prepended to the next tool result:**

> NOTE: logmon gained 4 tools since this conversation started:
> `snapshot_collector`, `diff_collectors`, `document_collectors`,
> `list_collectors`. See their descriptions.

Chosen because a tool result is the only channel guaranteed to land in the
agent's context. Not `logging/message` (client-dependent whether it reaches the
model). **Once**, not on every response: a banner on every call is a context tax
for a fact already delivered.

### 4.4 Removal mid-session

`remove_route` plus `list_changed`. A call to a removed tool errors; that is
correct and needs no special handling.

---

## 5. Generating the CLI

### 5.1 What is generated

Argument parsing and dispatch, via clap's **builder** API (`Command::new`,
`Arg::new`) rather than its derive macros, which are compile-time.

| Schema shape | Generated flag | vs. today |
|---|---|---|
| required scalar | `--name <NAME>` | identical |
| enum | `--level <LEVEL>` + `possible_values` | **better** — validated locally |
| array of scalars | `--group-key X` (repeatable, singularised) | identical |
| structured object | `--threshold-metric`, `--threshold-op`, … | identical |
| free-form object | `--meta k=v` (repeatable) | **better** — no hand-written JSON |
| anything else | `--params-json '{…}'` | new |

The point of the two "identical" rows: the friendly syntax is **the one the CLI
already uses**. Generation reproduces it rather than replacing it.

### 5.2 `--params-json`

Named to avoid `--json`, which already means *output* format on every command.
The blob is the **base**; typed flags **override** it. That supports pasting a
params object from a previous run and tweaking one field — which erroring on
overlap would block. Stated in `--help`, not left to be discovered.

### 5.3 What is not generated

Human-readable rendering. `format_trace_row` and friends pick columns, order and
formatting by hand, and no schema implies them.

This is not a regression, because **every command already supports `--json`**. A
generated command emits JSON; a hand-written renderer becomes an optional overlay
for commands that have one. A new daemon tool is therefore immediately reachable
from the CLI — today it is unreachable until someone writes the whole command.

### 5.4 Accepted regression

Shell completion of *command names* requires the daemon to be running, since the
command list comes from it. Flag and value completion within a command work once
the manifest is loaded.

### 5.5 The generator is not logmon-specific — and that is deliberately not built here

Nothing in §5.1 reads a logmon field. It consumes `name`, `description` and
`inputSchema` — the three things **every** MCP server publishes via `tools/list`.
An MCP client wrapping that generator would produce a CLI for any server, with
logmon merely the first.

That is a real and separable tool, and this design is shaped so it stays
reachable: because §3.2 uses the MCP `Tool` shape rather than a bespoke one, the
generator can later be pointed at `tools/list` instead of `tools.manifest`
without rewriting it.

**It is out of scope here, on purpose.** Building it means adding an MCP *client*
to a codebase that has none, plus server process spawning and lifecycle — while
the problem this design exists to fix (§0) needs none of that. Taking the
generality now costs a design constraint we would want anyway; taking the *scope*
now trades a fix for a framework.

If it is built later, start by surveying prior art rather than from scratch:
"generate a CLI from an OpenAPI/JSON-Schema description" is a well-travelled
genre, and this project's rule is to reconcile a new mechanism against a proven
reference rather than invent in parallel.

---

## 6. Hazards

### 6.1 Trust boundary — hard constraint

The daemon supplies text that lands **directly in an LLM's tool descriptions**.
Today that text is compiled into a binary the user installed deliberately.

**Constraint: local Unix socket only. Never remote, never multi-tenant.** Anything
that can write the daemon's manifest can write into an agent's instructions. This
is acceptable for a local, user-owned daemon and unacceptable the moment the
socket is not. Any future remote transport must re-open this decision.

### 6.2 Descriptions become a daemon release

Improving a description now ships with the daemon rather than the shim. A win for
§0 — the description travels with the capability and cannot drift from it — but
prose edits now require a daemon restart.

### 6.3 The migration is a relocation, not a de-duplication

34 `Params` structs live only in the shim (§1). They and their doc comments must
move to the protocol crate, and their `schemars`-derived schemas must be emitted
by the daemon. There is no existing copy to delete.

### 6.4 Two mechanisms during the cutover

Mitigated by §2's single-cutover choice: the shim's `#[rmcp::tool]` attributes are
deleted in the same change that adds the generic handler.

---

## 7. Test list

**Verification:** a manifest round-trips; a tool registered at runtime is
callable; the CLI generator produces the same flags as today's hand-written
commands for `collectors.add` (a golden comparison against the current
`--help`); `--params-json` merges under typed flags; enum validation fails
locally.

**Adversarial:** daemon down at initialize → zero tools + `instructions`, and
**no tool is callable**; daemon appears → tools register and `list_changed`
fires; daemon restarts with a tool removed → route removed; `format_version` too
new → refuse with a reason, serve nothing; one malformed descriptor → that tool
skipped, the other 41 registered; the banner appears **exactly once** across
several tool calls; an anonymous session → the shim refuses to start rather than
silently losing reconnect.

---

## 8. Open questions for the gate

1. Should `tools.manifest` be served to *any* session, or only a named one? It
   carries no user data, but it is the one RPC a shim must call before it can
   authenticate anything.
2. Does the banner survive a context compaction? If the agent's context is
   summarised away, the notice is lost and the tools are silent again. A cheap
   answer is to re-arm on reconnect rather than once per session.

---

# 9. Redesign — 2026-07-31, revision 2

**Status:** draft. **Not gated.** §§0–8 above are the withdrawn 2026-07-30 design, kept
for their reasoning; this section supersedes them wherever they conflict, and §9.8 lists
every subsection of theirs that is dead so a reader never has to derive that by
subtraction.

**Revision 2 supersedes revision 1 in the same file** (four lenses, ~40 findings). §9.9
records what changed and why, and is the shortest route to it.

**Counts corrected at `dfa37a5`:** **45** MCP tools and **45** dispatch arms (§1 says 42
and 43); **37** `*Params` structs in the shim, and — contradicting §1 and §6.3 outright —
**128 `JsonSchema` derives in `crates/protocol/src/methods.rs`**, with a committed
`protocol-v1.schema.json` regenerated by `cargo xtask verify-schema`. §1's *"zero in the
protocol crate"* is false, and §9.2 is about what that changes.

---

## 9.1 The goal, restated more precisely than §0 had it

§0's story is *"a stale shim cannot reach new tools."* True, and it is the smaller half.

**A current shim cannot reach parameters of tools it already has.** The daemon honours
`oneshot` on `triggers.add` (`rpc_handler.rs:1129`), `per_name` and `per_group` on
`collectors.snapshot` (`:2002`, `:2006`), `threshold` on `collectors.edit` (`:1933`),
`start_seq` and `description` on `bookmarks.add` (`:1568`), and `persist` on
`domains.create`. **None of them exist on the shim's params structs.** They exist in
`protocol/src/methods.rs`, which the SDK and the committed schema use and the shim does
not.

And 0.9.0's skew note reports *"reaches 45 of 45"* the entire time, because it compares
tool **names**. The mechanism built to make capability skew visible is structurally blind
to this kind.

So the goal is one goal, not two: **one description of each capability, authored where the
capability lives, reaching every consumer.** Tool reachability is one consequence;
parameter reachability is the other, and it is the one that is broken today.

## 9.2 Sequencing: assert before you delete

§2 says *"all 45 tools become daemon-taught, in one cutover"* and §6.4 deletes the
`#[rmcp::tool]` attributes in the same change. **That ordering is backwards**, and
inverting it converts the single largest risk into a test.

| Phase | What lands | What holds it honest |
|---|---|---|
| **A** | The daemon serves a manifest. Nothing consumes it. | Schema round-trip tests |
| **B** | The shim fetches the manifest and **asserts it matches the tools it already has** — names, and parameter sets field by field. Compiled-in tools still serve every call. | **The assertion itself.** The shim still has both descriptions, so the diff is mechanical |
| **C** | A release ships in that state. | Field use |
| **D** | Dynamic registration for tools the shim lacks; compiled-in retained for the rest. | §9.5 |
| **E** | Only once B has been quiet for a release: delete the attributes. | — |

**Phase B is the whole point.** §6.3 estimates the migration as *"move 34 structs"*; the
truth is *"reconcile two divergent 45-row tables"*, and 30 of the 45 pairs have drifted
(§9.1). Doing that by hand is 45 judgement calls with no checker. Doing it as an assertion
that fails loudly is 45 diffs a machine finds. **The design's riskiest step becomes its
cheapest one purely by ordering.**

It also gives a rollback that §2 does not have: through D, every tool still has a
compiled-in route, so a wrong-but-well-formed descriptor degrades to a mismatch warning
rather than a broken tool. §7 has no test for a well-formed-and-wrong descriptor, which is
the failure mode with no rollback under §2.

## 9.3 Validation: the daemon must, and the shim still should

Revision 1 concluded *"the daemon validates, the shim transmits — the shim carries no
validator."* The first half survives. The second does not, and the argument for it was a
non-sequitur.

**What holds.** The socket is open — verified: `srwxr-xr-x`, no peer-credential check
anywhere in the daemon's connection handling, and a raw `nc -U` session reaches
`logs.recent` in two lines. So a check that exists only in the shim protects nothing
against a socket peer, and the daemon's own validation is far worse than revision 1
credited (§9.4).

**What does not.** *"Shim-only validation is insufficient"* implies *"the daemon must
also validate"*. It does **not** imply *"the shim must not."* Revision 1 slid from the
first to the third.

**The distinction that was missed: the shim is not a security boundary for socket peers,
but it is the type boundary for the model.** An agent reaches the daemon *only* through
the shim. Deleting the shim's structural check does not relocate a check — it removes one,
on the only path the model has.

Two cases make it concrete, and both are free today:

- `snapshot_collector(reset: "false")` — a stringified boolean. Today `Option<bool>`
  refuses it before any RPC. Without it, `as_bool()` yields `None`, the default `true`
  fires, **the collector's live window is zeroed**, and success is returned. There is no
  error one round trip later; there is no error at all, and the round trip performed
  exactly the destruction the caller asked to avoid.
- `add_collector(group_keys: "cache.enabled")` — a scalar for an array, the most common
  shape error a model makes. Without the shim's check the daemon arms an **ungrouped**
  collector, persists it, and the A/B run cannot be split afterwards.

**So: both, and it is not the two-mechanisms defect.** The shim validates against **the
schema the daemon published**, so there is no second source of truth and nothing to
diverge — that was the real objection, and it is answered by provenance rather than by
deletion. One rule, checked at two boundaries with different consequences for failure:
the shim fails the call before any state changes; the daemon fails it because it cannot
trust who is calling.

**Revision 1's cost accounting was wrong in the same direction.** It claimed deleting the
shim's validator removes a dependency. rmcp validates nothing against `inputSchema` —
confirmed, no `jsonschema` dependency in rmcp 1.2.0, and the router validates only tool
*names* (`router/tool.rs:385`) — so a dynamic route needs a validator whether or not the
compiled-in ones keep theirs. The dependency arrives with dynamic registration; it is not
avoided by giving up the check.

## 9.4 The precondition, correctly counted

Revision 1: *"Ten sites read parameters as `params.get(k).and_then(as_u64).unwrap_or(d)`.
Small, but it gates the rest."* Both halves were wrong.

**~35 chains in `rpc_handler.rs`, in three shapes**, and the seven matching the quoted
pattern are the least dangerous:

| Shape | Sites | Consequence |
|---|---|---|
| `.unwrap_or(default)` | 7 | `count: "abc"` → 50 entries, no error. Proven live |
| **`None` selects *semantics*** | ~20 | A wrong-typed **`filter`** (8 sites) means **no filter** — the whole buffer instead of the matching subset. `reset`/`per_name`/`per_group`/`projections` default **true**, so a wrong type is destructive; `oneshot`/`merge`/`allow_*` default false and fail safe |
| False message | 16 | `.ok_or_else(\|\| "missing required parameter: X")` fires for a value that was **present**. Not a poor message — a false one |

Plus range and truncation, which no type check catches: `pre_window: 4294967296` truncates
via `as u32` to **0** — largest window requested, none captured, success reported. `id:
4294967297` addresses filter **1**. `before`/`after` cast `as usize` into `idx + after + 1`
and, with no `[profile.release]` overflow setting, wrap in release. This repo has already
paid for exactly this once — `CHANGELOG.md:482`, `skip_warmup_ms`.

**And the error surface cannot support what §9.5 asks of it.** Every handler error returns
`-32601`, method-not-found, with a bare string (`rpc_handler.rs:232`, `:244`). There is no
`-32602` in the daemon at all. So a shim cannot tell *"that tool no longer exists"* from
*"your filter is malformed"* by code — which is precisely the discrimination §9.5's
reconciliation needs.

**This precondition is being fixed independently of this design**, because it is a live
defect for every non-shim caller. See §9.10.

## 9.5 What the shim keeps, and why "presentation" was the wrong word

Revision 1: *"Reaching a capability is mechanism. Presenting its result is presentation.
One mechanism, always; presentation may be locally overridden, and its absence costs
nothing but prettiness — `--json` is always correct."*

**The line is real. The word was wrong, and it broke on the first hard case.**

`collectors document --path` writes **two** files client-side — the document and its
sidecar, the second at a path computed from the first. The daemon cannot do it, and the
code says why (`cli/collectors.rs:655-690`): *"the broker runs as a service, so a relative
path would resolve against its working directory rather than the caller's."* Consequences
revision 1's binary has no box for:

- `--path` is **not a wire parameter**, so a CLI generated from `inputSchema` cannot
  produce the flag at all. The capability disappears, not its prettiness.
- `--json` is **not** "always correct": it prints the markdown as an escaped string field
  and writes nothing.
- With no renderer the **sidecar is never written**, while the document's front-matter
  still names it. That is correctness, not output quality.

Same shape at `logs export --out` and `export_logs`'s `path`/`format`, neither of which is
a wire parameter either.

**And there is a second thing the daemon structurally cannot do — session scoping.** The
CLI re-binds the domain before every invocation (`cli/mod.rs:41-55`) because it connects as
a *persistent named session*. On the wire, *"no `--domain` on this invocation"* and *"leave
my binding alone"* are **indistinguishable**. Delete that client-side bind and:

```
logmon-mcp --domain prod logs recent    # binds prod, server-side, persistently
logmon-mcp logs clear                   # today: re-binds default, clears default
                                        # generated: inherits prod, CLEARS PROD
```

`logs.clear` takes no parameters at all; its entire scope comes from that binding. The
classification is also three-way and per-tool — `is_registry_op` (`cli/domains.rs:28`)
exempts `create`/`delete`/`list`, while `clear` **must** honour the bind — and no field in
§3.2's descriptor can express any of it.

**So the boundary is drawn differently:**

> **The wire describes what a capability is. The client owns everything that happens on
> the client** — where a file lands, what a session is bound to, how a result reads.
> A generated surface may not silently drop a client-side effect; a tool that has one
> keeps a local implementation, and the manifest must be able to say so.

That is a real constraint on the descriptor, not a philosophical line: **a descriptor needs
a field marking a tool as having client-side behaviour**, so the generic path refuses to
handle it rather than handling it wrongly. Three tools have one today — `export_logs`
(writes a file), `get_status` (injects `shim_note`), `document_collectors` (composes text
and writes a sidecar) — and §1's *"42 of 42 passthrough, one exception"* was arrived at by
counting `.call(` sites, which cannot distinguish a passthrough from a body that also
writes a file.

## 9.6 What survives of "tool-independent", honestly

Revision 1: *"the socket path, the auto-start command, and the server name"* become
configuration. That undercounts. Also logmon-specific and **not** constants:

- **The notification map.** `notifications.rs:9` — `TRIGGER_FIRED_METHOD`, with
  `Notification::TriggerFired` re-serialized and `Reconnected` deliberately dropped.
- **The domain bind** (§9.5), including its three-way per-tool classification.
- **`LOGMON_DOMAIN`** and its fail-loud contract, documented in the README and skill.
- **Mode-dependent transport** — `connect_cli` sets zero reconnect attempts and a 5s
  timeout and sends per-subcommand `argv` in `client_info`; MCP mode uses reconnecting
  defaults.
- **`auto_start.rs` alone** hardcodes ten-plus identities: the binary name across three
  lookup tiers, `daemon.lock`, `daemon.pid`, `logmon.sock`, `autostart.log`, `config.json`,
  the `LOGMON_CONFIG_DIR` policy, a Windows `127.0.0.1:12200` fallback, and remediation
  text naming `cargo install --path crates/broker`.

**So the honest claim is narrower and still worth having:** the *tool surface* becomes
daemon-taught. The *adapter* stays logmon-shaped. Reuse by another daemon needs the
notification map and a per-tool pre-call hook to become wire concepts too — real work,
not a repackaging — and §9.11 keeps it as a question rather than a promise.

## 9.7 Bootstrap, corrected

Revision 1's ladder had a row missing and a row that cannot work.

**`main.rs` has two `?`-exits before `serve()`, not one** — `:104`
(`ensure_broker_running`) and `:116` (`builder.open()`) — plus a third failure class at
`:114`: **the bound domain does not exist**, fail-loud by design. The daemon is up and
healthy, so *"daemon unreachable, start it"* is the wrong message and a cached manifest
would advertise tools that all fail for an unrelated reason. Three failure classes, three
remediation texts.

**The skill text cannot ride this ladder at all.** `get_info()` is a **synchronous** fn
called inside `serve()` while composing the initialize response, MCP has no
instructions-changed notification, and rmcp's `ServerNotification` enum has none. So tools
are recoverable after a late fetch via `list_changed`; instructions are not. Whatever
`get_info()` had at initialize is what that session has forever. Revision 1's *"a skill
file that cannot go stale"* is unsupported by any mechanism in this document, and §9.11
carries it as an open question rather than a claim.

**And §3.5 must not survive.** It refuses a too-new `format_version` and **serves no
tools** — the exact remedy §9's own cache argument rejects, and applied to §0's motivating
case it makes it *strictly worse*: Store's three-versions-behind shim gets 45 of 45 tools
plus a skew note today, and zero tools under §3.5. Adopt §3.6's shape instead — skip what
you cannot parse, keep the rest, say what was skipped.

## 9.8 Which of §§0–8 are dead

Revision 1 named four of twenty-five subsections and left a reader to derive the rest by
subtraction. Stated:

| | Status under §9 |
|---|---|
| §2 (one cutover) | **Dead** — §9.2 inverts it |
| §3.5 (refuse a newer format, serve nothing) | **Dead** — §9.7 |
| §4.1 (zero tools on bootstrap) | **Dead** — §9.7 |
| §5.1's "identical" rows | **Dead** — §9.11 Q4; the table has no row for positionals, variadics or booleans, which is most of this CLI |
| §6.3 ("no existing copy to delete") | **Dead** — §9.1; 128 derives say otherwise |
| §7's "daemon down → no tool is callable" | **Dead** — contradicts §9.2 phase D |
| §1's tool/method/struct/line counts | **Superseded** — see this section's header |
| §1's "42 of 42 passthrough, one exception" | **False**, and was false at `98e75cc` — §9.5 |
| §3.2's descriptor shape | **Live, with two changes** — `x-logmon` → MCP's `_meta` (§9.11 Q1), plus the client-side-behaviour marker §9.5 requires |
| §3.6 (skip a bad descriptor, keep the rest) | **Live, and promoted** — §9.7 applies it to §3.5's case too |
| §6.1 (local socket only, never multi-tenant) | **Live, and now load-bearing** — §9.11 Q2 |
| §1's rmcp seams | **Live and re-verified** against rmcp 1.2.0, line for line |
| Everything else in §§0–8 | Live as reasoning, not as a plan |

## 9.9 What revision 2 changed

| Revision 1 said | Revision 2 says | Because |
|---|---|---|
| The shim carries no validator | Both validate; the shim against the daemon's published schema | `reset: "false"` silently zeroes a window; the shim is the model's only path |
| §9.3 also listed "schema validation" as compiled in | — | It contradicted the above outright; both were mine, from two drafts |
| Ten precondition sites | ~35, in three shapes, plus range | The quoted pattern was the least dangerous of the three |
| One cutover | Assert, ship, then delete | Turns 45 hand judgements into a test, and gives a rollback |
| "Presentation may be locally overridden" | The client owns client-side effects; descriptors must say which tools have them | `--path` writes two files the daemon cannot write |
| Three constants generalize the shim | The tool surface generalizes; the adapter stays logmon-shaped | Notifications, domain bind, `LOGMON_DOMAIN`, transport mode |
| A skill file that cannot go stale | Open question | `get_info()` is sync, called once, with no update channel |
| One `?`-exit | Two, plus a third failure class | `builder.domain()` fails loud when the domain is gone |

## 9.10 Preconditions, both being handled outside this design

1. **Daemon-side parameter validation** (§9.4) — ~35 sites, in progress as its own change
   because it is a live defect for every non-shim caller, independent of this design.
2. **The `?`-exit path** (§9.7) — `GelfMcpServer` holds `broker: Broker` by value
   (`server.rs:12`), so making it optional propagates to every route. Small in `main.rs`,
   not small in the struct.

## 9.11 Open questions for the gate

1. **`_meta`, not `x-logmon`.** `rmcp::model::Tool` is `#[non_exhaustive]` with no
   catch-all, so `from_value::<Tool>` **silently drops** an `x-` key and the routing method
   vanishes. MCP's own extension channel is `_meta` — which is exactly the
   "field names fixed by a published spec rather than by us" property §3.2 argued for and
   then did not use. Confirm and rewrite §3.2.
2. **§6.1's trust boundary is stated over the transport and needs restating over daemon
   identity.** `LOGMON_BROKER_BIN` and `LOGMON_CONFIG_DIR` are settable from an MCP config
   `env` block, so *which daemon* is already influenceable. Today a substituted daemon
   yields fake log **data**; under this design it authors tool names, descriptions,
   schemas, method routing and `instructions` — and `annotations` are pure loosening, since
   there are none today, so a descriptor claiming `readOnlyHint: true` while routing to
   `domains.clear` would be auto-approved by a client that trusts the hint.
3. **Does the cache survive at all?** §9.2's phase D leaves every tool with a compiled-in
   route, which removes the cache's main job. If it stays: its key must be the resolved
   socket path, not `config_dir()` — 22 CLI integration tests set only
   `LOGMON_BROKER_SOCKET`, so a config-dir-keyed cache would have every parallel test read
   and write the developer's real `~/.config/logmon`.
4. **Can the CLI be generated at all?** §5.1's table has no row for positionals
   (`collectors diff a b`), variadic positionals (`collectors document base after`),
   booleans (13 flags — a JSON Schema `"boolean"` under §5.1's rule generates
   `--merge <MERGE>`), negated names (`--no-reset` for wire field `reset`), or cross-field
   requirements. And `triggers add`'s CLI defaults are **deliberately not** the daemon's —
   0/0/0 versus 500/200/5, pinned by `crates/core/tests/trigger_window_defaults.rs` — so
   regeneration would silently change every such invocation. Only ~17% of the CLI's 2713
   lines are argument definition; ~45% is rendering. **The honest answer may be that the
   CLI is out of scope and only the MCP surface is daemon-taught.**
5. **What happens to 0.9.0?** `mcp_tools::TOOLS`, `annotate_skew`, `shim_note` and the two
   source-scanning tests all anchor on compiled-in tool names. Phase D keeps them working;
   phase E removes their anchor. Note that tool-name skew becoming impossible does **not**
   cover daemon-binary-versus-daemon-process skew, which `auto_start.rs`'s three-tier
   lookup makes reachable.
6. **Is the generic core a separate crate?** §9.6 makes the reuse claim narrower but not
   zero, and it is only real if someone can depend on the core without depending on
   logmon — today `crates/mcp` depends on `logmon-broker-core` solely to reach
   `config_dir`.
7. **Does `serve()` block on the first fetch?** §9.7 says instructions cannot arrive late.
   Blocking makes every client pay the daemon's latency at initialize — and `auto_start`
   already waits up to 10s for a socket. Not blocking means the first run after install is
   the one run with no skill text, which is the run that needs it most.

---

# 10. The bundled manifest — 2026-07-31, revision 3

**Status:** draft. Not gated. Supersedes §9.2, §9.7 and §9.11 Q3–Q4.

Revision 2 treated the shim's compiled-in tools as **legacy to be deleted**, which forced
every hard question: what happens at bootstrap, where does the cache live, how does
`--help` work offline, what becomes of 0.9.0's skew detection. All four dissolve under one
change of framing.

> **The shim ships a manifest. The daemon serves a manifest. They are the same artifact
> from two sources.**

The compiled-in tools are not legacy. They are the **bundled manifest** — generated at
build time from the same protocol types the daemon generates its own from, embedded the
way `SKILL_INSTRUCTIONS` already is (`server.rs:1504`).

## 10.1 What this makes free

| Question revision 2 had to answer | Answer now |
|---|---|
| Bootstrap with no daemon (§9.7) | The bundled manifest. **Exactly today's behaviour**, because it is today's tool set |
| Where does the cache live (§9.11 Q3) | It does not have to exist. The bundled manifest is the floor; a runtime cache is an optimisation, not a mechanism |
| `--help` offline | Works, at today's quality, from the bundle |
| What happens to 0.9.0's skew note (§9.11 Q5) | **It improves.** `bundled ⊖ fetched` is the skew, computed over *parameters* rather than tool names — which is exactly the blindness §9.1 identified |
| The phase-B assertion (§9.2) | Not a migration step that gets deleted. It is the **permanent** comparison, and it is how the note is computed |
| "One cutover" vs incremental | Neither. Nothing is ever deleted; a second source is added |

Phase E of §9.2 — *"delete the attributes"* — becomes **"replace the attributes with a
generated bundle"**. No capability is ever removed, so there is no step with no rollback.

**Merge rule.** Union, fetched descriptor winning for a tool in both. A tool only in the
bundle stays registered and is reported as *not advertised by this daemon* — never
unregistered, because a rename is a removal plus an addition and unregistering makes the
old name vanish before the agent learns the new one. A tool only in the fetch is registered
dynamically. Both directions are the skew note's content.

## 10.2 The CLI, now that its syntax may change — **DEAD (gated 2026-08-01)**

> **This section did not survive its gate. Do not build from it.** Two reasons, both
> measured, both in §11.5: the derivation table reads schema features this project's schema
> does not have (`enum` on a request param — **0**; `"default": true` — **0**;
> `dependentRequired` — **0**; `"type":"boolean"` matches **2 of 16** booleans), and it
> would make a broker upgrade able to change how the CLI parses argv. Kept because the
> reasoning about *why* reproducing a hand-written surface is the wrong question is still
> right — it is the answer that was wrong.

§9.11 Q4 asked whether the CLI can be generated, and answered *"probably not"* — because
JSON Schema cannot express `collectors diff a b`, `--merge` as a presence flag,
`--no-reset`, or `requires_all`.

**That framing was wrong. It asked whether generation can reproduce a surface that was
hand-written under no constraint.** Once the surface may change, the question becomes: what
CLI does a schema describe *well*? The answer is: nearly all of one, by convention, with a
short override vocabulary for the rest.

### Derivation, needing no metadata at all

| From the schema | Becomes |
|---|---|
| the RPC method, `.` → space | the command path: `collectors.diff` → `collectors diff` |
| a property name, snake → kebab | `group_keys` → `--group-keys` |
| `"type": "boolean"` | a presence flag `--x`; plus `--no-x` when the schema's default is `true` |
| `"type": "array"` of scalars | a repeatable singular flag — `--group-key` used N times |
| `"type": "object"` | prefix-flattened — `threshold.metric` → `--threshold-metric` |
| `"enum": [...]` | clap `possible_values` — **validated locally, and listed in `--help`** |
| `"default": v` | the flag's default |
| `"description"` | the flag's help text |
| `dependentRequired` | clap `requires_all` |

Every shape §9.11 Q4 listed as impossible is on that table. The obstacle was never
expressiveness; it was the requirement to reproduce choices made without a schema in mind.

**Two of these rows are repairs, not ports.**

- **Enums become validated.** §1 records that `--level tre` passes clap today and fails at
  the daemon, because enums live in prose. A generated CLI checks them, offline, and shows
  them in `--help`.
- **Defaults stop diverging.** `triggers add` defaults to `0/0/0` in the CLI and `500/200/5`
  in the daemon — a divergence a test currently *pins*
  (`crates/core/tests/trigger_window_defaults.rs`). Under generation the schema's default is
  the only default, so that class of bug cannot exist. The gate reported this as
  *"regeneration would silently change every such invocation"*; that is true, and the
  change is a fix. It is a breaking CLI change and belongs in the changelog as one.

### The override vocabulary, for what convention gets wrong

In the descriptor, and **empty for most tools**:

```json
"cli": {
  "path":       ["collectors", "diff"],   // when it should differ from the method
  "positional": ["a", "b"],               // collectors diff base@* new@*
  "variadic":   "names",                  // collectors document base after
  "hidden":     ["internal_flag"]         // reachable via --params-json only
}
```

Four keys. `collectors diff` and `collectors document` need one each; on the current
surface nothing else needs any.

### Rendering, by the same shape

MCP descriptors carry an **`outputSchema`** (`rmcp/src/model/tool.rs:30`), which this
project does not use. With it, a generic renderer can do better than raw JSON:

```json
"cli": { "table": ["seq", "level", "message"] }
```

— a generic table renderer for the ~45% of CLI lines that are presentation, with **bespoke
renderers remaining as local overrides** where they earn it (§9.5's rule is unchanged: the
client owns what happens on the client). A tool with no hint and no local renderer prints
JSON, which is correct if plain.

## 10.3 Why this is one mechanism and not two

The obvious objection: bundled *and* fetched is two sources, which is the shape this
project keeps being burned by.

It is not, and the test is whether they can **disagree about the same thing without anyone
noticing**. They cannot:

- They are the **same format**, generated by the **same code** from the **same types** —
  the bundle is `cargo xtask` output, the daemon's is that function called at runtime.
- Their difference is not a bug to be avoided; it is **the product**. `bundled ⊖ fetched`
  is precisely the skew this whole design exists to surface, and it is reported on every
  `status.get`.
- A divergence is therefore loud by construction. The failure mode of two sources of truth
  is silence, and silence is the one outcome this arrangement cannot produce.

Compare the situation it replaces: three descriptions of every parameter — the shim's
`Params`, the protocol crate's type, and the daemon's `params.get()` reads — which
disagreed in 30 of 45 pairs **for months**, undetected, because nothing compared them.

## 10.4 What this does not solve

- **§9.5's client-side behaviour** stands unchanged. `export_logs`, `get_status` and
  `document_collectors` do things no descriptor can delegate, and the descriptor must mark
  them so the generic path refuses rather than mishandles.
- **The domain bind** (§9.5) stands. A per-tool pre-call hook is still needed, and it is
  still logmon-specific.
- **§9.3's validation** is unchanged, and gets simpler: the shim validates against whichever
  manifest it is using, and there is always one.
- **§9.4's precondition** is unchanged and is being fixed independently (§9.10).
- **The reuse claim** (§9.6) is unchanged: the tool surface generalizes, the adapter does
  not.

## 10.5 Open questions for the gate

1. **Does the bundle generate cleanly from the protocol crate?** §9.1 says 30 of 45 pairs
   have drifted, and the bundle must be generated from *one* of them. Generating from the
   protocol types means the bundled manifest is **not** what today's shim exposes — it is
   what the daemon accepts, which is larger by six parameters. That is the right target and
   it means the first bundle is itself a behaviour change.
2. **`cli` under `_meta`, or a sibling?** §9.11 Q1 established that `x-` keys are silently
   dropped by `rmcp::model::Tool`, so it must ride `_meta`. Whether the CLI block belongs in
   the same envelope as the routing method or beside it is a wire-shape decision.
3. **Does `--params-json` survive?** With `hidden` in the override vocabulary it becomes the
   escape hatch for deliberately-unexposed parameters, which is a narrower and more
   defensible role than revision 1's "anything the generator could not handle".
4. **Is the breaking CLI change acceptable in one release?** Positional-to-flag moves,
   changed defaults, and enum validation are all user-visible. The bundled manifest means
   they can be staged — but staging them means shipping a generated CLI that deliberately
   reproduces the old surface for a release, which is the "reproduce a hand-written surface"
   trap this section exists to escape.

---

# 11. The plan — 2026-08-01

**Status:** the live plan. §§0–8 withdrawn, §9 partly live (§9.3–§9.6), §10 live except
**§10.2, which is dead** — see §11.5.

Written architect-then-reviewer to convergence before anyone read it; §11.6 records what
that pass changed, because a plan that claims a method should show its work.

## 11.1 What is actually broken, measured

Not "a stale shim cannot reach new tools" (§0). That is the smaller half and it is already
*visible* — 0.9.0's skew note reports it. The larger half is live, silent, and costing
something today:

| | Measured at `7a61856` |
|---|---|
| Tool descriptions with no home in the protocol crate | **45** — all in `#[rmcp::tool(description=…)]` attributes in the shim |
| Request parameters whose legal values live in **prose** rather than in the schema | **8** (`level` ×2, `group_by` ×3, `format`, `threshold.metric`, `threshold.op`) |
| Tools in `mcp_tools::TOOLS` with **no schema at all** | **1** — `rename_session`; `SessionRename` occurs **0** times in `protocol-v1.schema.json` because `xtask`'s type list is hand-maintained |
| Parameters the daemon honours that the shim cannot send | **8**, across 7 methods |
| Statements in `skill/logmon.md` that are **false** because of the above | **4** |

The last row is the one that matters. `crates/mcp/src/server.rs` has **no
`deny_unknown_fields`** on any params struct, so an agent following the skill sends
`oneshot`, serde drops it, and the call returns **success**. A permanent trigger, reported
as the one-shot the agent asked for.

**None of that needs a manifest to fix**, which is why §11.2 comes first and can ship alone.

## 11.2 Phase 0 — stop the bleeding (no design required)

1. **Parameter typing** — absent vs present-but-wrong-typed, ~35 sites. *Landed.*
2. **`deny_unknown_fields`** on the shim's param structs, so a parameter the shim cannot
   send is an error rather than a silent drop.
3. **A drift test** between the shim's `*Params` and the protocol crate's request types.
   This is the guard whose absence let 8 parameters diverge unnoticed; it is ~40 lines.
4. **Add the 8 parameters**, and correct the 4 skill statements.

**Exit:** the skill is true, and drift cannot recur silently. **This may be the whole
project** — afterwards the residual problem is only that a *new* daemon tool needs a shim
rebuild, which 0.9.0 already makes visible. Decide whether to continue *after* this, with
the motivation measured rather than assumed.

## 11.3 Phase A — make the schema sufficient

Only if Phase 0's residue justifies continuing.

- **The 8 prose-enums become Rust enums.** Wire-compatible, and verified rather than
  assumed: `Level` already does exactly this and schemars emits
  `"type":"string","enum":["Trace",…]`. Serde's output is unchanged, so no client sees a
  difference.
- **The 45 tool descriptions are authored in the protocol crate.** They are §0's "product",
  and today they live where a daemon cannot reach them.
- **`xtask`'s hand-maintained type list is replaced or completed**, so a tool cannot have no
  schema. `verify-schema` cannot catch this today: it compares the schema against the list
  it was given, so an omission is invisible from both sides.

**Exit, and it is a query rather than a judgement:** zero request parameters with
prose-only alternatives; every tool in `TOOLS` resolves to a schema.

**Worth alone:** enum validation for the SDK and every schema consumer, and the
`rename_session` hole closed. Ships without anything below it.

## 11.4 Phase B — one generator, two callers, no behaviour change

`protocol::manifest()` builds descriptors from schemas + `TOOLS` + descriptions.
`schema_for!` is an ordinary **runtime** call (`schemars/src/macros.rs:43-46`), so `xtask`
serialises it to a committed file and the daemon calls the same function. The shim fetches
it and **asserts it matches the tools it already has**.

**Phase B does not end when the assertion exists. It ends when the assertion is clean** —
which means Phase 0's drift is resolved, not merely detected. An earlier draft of this plan
had Phase C building on the compiled-in tools as a fallback while they still disagreed with
the manifest by 8 parameters; the fallback would have been known-inconsistent from the
first commit.

**What the assertion is and is not.** It compares a build-time snapshot against a runtime
call of the same function, so it detects **version** skew at parameter resolution. It does
**not** validate either against the daemon's actual `params.get()` reads — 33 handlers read
raw, and the repo says so in its own words at `crates/core/tests/trigger_window_defaults.rs:34-36`.
Phase 0 item 1 narrows that gap; nothing here closes it.

## 11.5 Phase C — the shim registers from the manifest, and D is a separate decision

**C.** `tool_router` becomes a shared handle so registration can happen after `serve()`,
which takes `self` by value today. `#[tool_handler]` generates a **synchronous** `get_tool`
alongside an async `call_tool` returning a `Send` future, so neither `std` nor `tokio`
`RwLock` works — it is `ArcSwap` (**a new dependency**, zero occurrences in `Cargo.lock`) or
three hand-written handler methods. Compiled-in becomes the fallback, fetched the override,
union. The shim is then tool-independent **for MCP**. The three non-passthrough tools
(`export_logs`, `get_status`, `document_collectors`) keep local implementations and are
marked as such in the descriptor (§9.5).

**D — generating the CLI — is dead as specified in §10.2, and is a separate decision even
after Phase A.** Two independent reasons:

1. **The derivation table read features this schema does not have.** Measured: `enum` on a
   request parameter — **0**; `"default": true` — **0**; `dependentRequired` — **0**;
   `"type":"boolean"` matches **2 of 16** booleans, because schemars renders `Option<bool>`
   as `["boolean","null"]`. Phase A fixes the first; it does not fix the rest.
2. **It makes a broker upgrade able to change how the CLI parses argv.** The CLI connects on
   every invocation and fetched-wins, so flag names and defaults would come from the running
   daemon. Today clap parses before the daemon is contacted at all. A CI script pinned to
   one shim version could stop parsing because someone upgraded the broker. That is a new
   failure mode with no precedent here, and §10 did not list it.

If D is ever wanted it needs its own design, its own deprecation period, and an answer to
(2). "The CLI syntax may change" removes the *compatibility* objection; it does not remove
the *coupling* one.

## 11.6 What the internal review changed

Recorded because the method is new and its value is testable.

| Architect wrote | Reviewer found | Fix |
|---|---|---|
| "Rust enums are wire-compatible" | Asserted, not checked | Opened the schema: `Level` emits `"enum":[…]` and serde is unchanged. **Confirmed** |
| "~10 prose-enums" | A number with no command behind it | Queried the schema: **8** |
| Phase C falls back to the compiled-in tools | **They still disagree by 8 parameters at that point** | Phase B ends when the assertion is *clean*, not when it exists |
| The prose-enums are a defect | Might have been a choice — forward-compat for new levels? | The daemon already hard-errors on an unknown level, so a loose schema buys nothing. **Drift, not design** |
| ArcSwap "or a hand-written handler" | A new dependency stated as an aside | Named as a cost in §11.5 |

Two of these — the number and the Phase B/C gap — are the classes that cost the most in the
2026-07-31 gate. Catching them at the desk is the whole point.
