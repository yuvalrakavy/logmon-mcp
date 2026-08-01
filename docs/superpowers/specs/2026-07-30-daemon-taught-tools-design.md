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

**→ §11 IS THE PLAN, and §11.0 IS THE GOAL. Start there.** Everything above is how it was
arrived at.

§10 (revision 3) was gated by four lenses on `7a61856`. **§10.2 — generating the CLI — did
not survive** and is dead (§11.5). The rest of §10 stands.

**One framing correction that supersedes every "is this worth it" argument in §§0–10.**
Those sections all argue from *defects* — drifted parameters, a lying skill, tools a stale
shim cannot reach — and Phase 0 (§11.2) fixes every one of them. That made the natural next
question *"is the rest still needed?"*, and it is the wrong question. **The goal is
architectural** (§11.0): cut the shim's dependency on the daemon, and make the daemon the
single point of truth. No number of defect fixes reaches that, so Phase 0 removing the pain
says nothing about whether to continue.

**Scope: being logmon-agnostic is not a current goal** — the architecture only has to leave
it *possible*. The plan is **A → B → C**; "generic" is a property to preserve, not a phase
to build (§11.0).

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

**And the error surface cannot support what §9.5 asks of it.** Every *handler* error returns
`-32601`, method-not-found, with a bare string (`rpc_handler.rs:232`, `:244`). So a shim
cannot tell *"that tool no longer exists"* from *"your filter is malformed"* by code — which
is precisely the discrimination §9.5's reconciliation needs.

**Correction, 2026-08-01:** an earlier version of this paragraph said *"there is no `-32602`
in the daemon at all."* False — `server.rs:870`, `:885`, `:894` emit it for handshake-time
invalid params. The claim was made by grepping `rpc_handler.rs` alone, and **two gate lenses
repeated it from the same narrow grep.** The corrected fact is narrower and still supports
the point: `-32602` exists but no *handler* uses it. Recorded because the error is a
negative asserted without widening the search — the exact check this project's own
self-review list names, and three readers walked past it.

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

## 11.0 The goal, stated by the user 2026-08-01 — and it is architectural

Every earlier version of this document argued from **defects**: a stale shim cannot reach
new tools, parameters have drifted, the skill lies. Phase 0 fixes all of that, which made
the obvious question *"is the rest still worth it?"* — and that question was aimed at the
wrong target.

> **Cut the dependency between the shim and the daemon. Enhance the daemon without
> reinstalling the shim. One point of truth: the daemon. The shim becomes generic — no
> particular knowledge of logmon — and usable by other daemons.**

That is a goal about **architecture**, not about a bug count, and it is not reachable by
fixing defects however many you fix. So the residue argument does not apply to it: Phase 0
was never going to satisfy this, and the fact that Phase 0 removes the *pain* is beside the
point.

**What this changes about the plan.** Phases A–C stay, and stop being conditional. They are
are **not sufficient for the fourth clause**, and §9.6 already measured why: the tool
*surface* becomes daemon-taught, while the *adapter* stays logmon-shaped. Still compiled in
after Phase C —

- the notification map (`notifications.rs:9`'s `TRIGGER_FIRED_METHOD`, with `Reconnected`
  deliberately dropped),
- the per-invocation domain bind and its three-way per-tool classification (§9.5),
- `LOGMON_DOMAIN` and its fail-loud contract,
- mode-dependent transport (CLI: zero reconnect attempts, 5s timeout, per-subcommand
  `argv`; MCP: reconnecting defaults),
- and `auto_start.rs`'s ten-plus hardcoded identities — binary name across three lookup
  tiers, `daemon.lock`, `daemon.pid`, `logmon.sock`, `autostart.log`, `config.json`, the
  `LOGMON_CONFIG_DIR` policy, a Windows port fallback, and remediation text naming
  `cargo install --path crates/broker`.

**Scope, settled 2026-08-01: being logmon-agnostic is NOT a current goal. The architecture
making it possible is enough at this stage.** So the plan is **A → B → C**, and there is no
Phase E in it.

The list above is therefore not a work item; it is the **test** the architecture has to
pass. For each entry, A–C must leave it *extractable* — reachable through a seam rather than
welded into the tool path — without extracting it. Concretely: the notification map and the
domain bind stay logmon-specific and stay where they are, but nothing in the manifest, the
registration path or the descriptor may come to *depend* on them. If a later phase would
have to unpick the tool surface to make the adapter generic, A–C got the seam wrong, and
that is a design error to catch now rather than a feature to build now.

The distinction matters because it changes what "done" means for A–C: not "the shim is
generic" — it will not be — but "nothing new was welded in."

Phase D — generating the CLI — remains dead (§11.5), and it is worth being clear that this
goal does **not** revive it. A generic *MCP adapter* is reachable. A generic CLI is a
different artifact and the measurements in §10.2's header still stand against it.

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

**Exit:** the skill is true, and drift cannot recur silently. Phase 0 removes the *pain*,
**not the goal** (§11.0) — the architecture is the point, and Phase 0 does not touch it.

An earlier version of this paragraph said Phase 0 *"may be the whole project"*, on the
grounds that afterwards the only residue is a new daemon tool needing a shim rebuild, which
0.9.0 already makes visible. That reasoning was sound and aimed at the wrong target: it
weighed the remaining *pain*, and the goal is not pain relief. Kept as a record because the
error is instructive — a plan that argues from defects will always conclude "stop" once the
defects are gone, whatever the design was actually for.

What Phase 0 does buy for A–C is that they no longer race a live defect, and that the drift
test becomes the mechanical checker Phase B's assertion needs (§11.4).

## 11.3 Phase A — make the schema sufficient

Only if Phase 0's residue justifies continuing.

### 11.3.1 Revision 2, 2026-08-01 — the enum plan was wrong, and measuring it is what showed that

Revision 1 said *"the 8 prose-enums become Rust enums."* Both halves are false. Grounding
each field in the parser that actually reads it — rather than in the doc comment beside it
— produced this table. **The daemon parses raw `Value` for all ten**: `rpc_handler`
deserializes into a protocol struct for exactly four methods (`DomainsCreate`,
`DomainsDelete`, `SessionRename`, `DomainsUse` — `rpc_handler.rs:292/403/455/508`), and
none of those four contains any field below.

| # | Request field | What the daemon actually accepts | Parser | Doc comment says | Δ |
|---|---|---|---|---|---|
| 1 | `TracesSlow.group_by` | `name` groups; **any other string** silently returns ungrouped | `rpc_handler.rs:1320` | same | — open set |
| 2 | `ThresholdSpec.metric` | `count`, `total_ms`, `avg_ms`, `error_count`, `error_rate_pct` | `threshold.rs:72` | same 5 | — closed |
| 3 | `ThresholdSpec.op` | `gt`/`>`, `gte`/`>=`, `lt`/`<`, `lte`/`<=` | `threshold.rs:120` | the 4 word forms only | **omits 4 aliases** |
| 4 | `CollectorsAdd.level` | `scalar`, `timing`, `tree` | `rpc_handler.rs:1651` | same 3 | — closed |
| 5 | `CollectorsEdit.level` | `scalar`, `timing`, `tree` | `rpc_handler.rs:1797` | same 3 | — closed |
| 6 | `CollectorsGet.group_by` | `none`/`""`, `name`, `group`, `trace`, `path` | `project.rs:72` via `rpc_handler.rs:2314` | omits `none`, `""` | **omits 2** |
| 7 | `TracesProfile.group_by` | identical — same `profile_options` | `project.rs:72` via `rpc_handler.rs:2314` | **no doc comment at all** | **undocumented** |
| 8 | `CollectorsDiff.group_by` | `none`/`""`, `name`, `group` | `diff.rs:75` via `rpc_handler.rs:2029` | omits `none`, `""` | **omits 2** |
| 9 | `CollectorsDocument.group_by` | `none`/`""`, `name`, `group` | `diff.rs:75` via `rpc_handler.rs:2092` | omits `none`, `""` | **omits 2** |
| 10 | `CollectorsDocument.format` | `md`/`markdown`/`""`, `json`, `folded`/`collapsed` | `document.rs:56` | `md`, `json`, `folded` | **omits 3 aliases** |

**Ten, not eight** — the "8" was never measured. And **six of the ten have prose that
misstates the accepted set.** The justification for Phase A is therefore stronger than
revision 1 claimed, but its *mechanism* was wrong in a way that would have broken the wire:

> Converting these ten to Rust enums would have **narrowed five of them** — dropping `>`,
> `>=`, `<`, `<=` from `op`; dropping `none` and `""` from four `group_by` fields; dropping
> `markdown`, `collapsed` and `""` from `format` — and would have made `traces.slow`
> hard-error where it deliberately passes. Five wire-contract breaks, from following the
> plan as written.

`#[serde(alias)]` does not rescue this: aliases affect deserialization only, so schemars
would still emit the narrow set and the schema would be **narrower than the daemon** — a
CLI validating against it would reject input the daemon accepts.

### 11.3.2 What Phase A actually delivers: an accurate schema, not Rust enums

The deliverable was never the enums; it was **a schema that exactly describes what the
daemon accepts**. Rust enums were one means, and for six of the ten they are a worse
means, because they cannot express the real accepted set. So:

- **`#[schemars(extend("enum" = [...]))]` on the existing `String`/`Option<String>` fields**,
  listing the accepted values *including every alias*, transcribed from the parser. One
  mechanism for all ten. **No Rust type changes, no wire change, no daemon change** — and
  the schema becomes exactly as wide as the daemon, in both directions.

  **Probed, not assumed** (schemars **1.2.1** — the version actually resolved in
  `Cargo.lock`, not the 1.2.2 an earlier note claimed). A scratch crate deriving the real
  field shapes confirms `extend("enum" = [...])` emits `"enum"` beside `"type"`, and that
  `extend("default" = true)` on an `Option<bool>` emits `"default": true` — which is what
  closes the `persist` question with no type migration.

  The probe also caught a defect that inspection would not have. On an `Option<String>`,
  schemars widens `"type"` to `["string","null"]` but **does not add `null` to the `enum`
  list** — so a validator would reject `group_by: null`, which the daemon accepts as
  absent (`opt_of` maps `Some(Value::Null)` → `None`). That is precisely the
  narrower-than-the-daemon false positive this phase exists to avoid, and it is the shape
  that caused the `edit_collector` production bug: `json!` renders `None` as `null`, so
  `null` genuinely appears on the wire. Hence the rule:

  | Field shape | Declaration |
  |---|---|
  | required `String`, closed set | `extend("enum" = [ …values… ])` |
  | `Option<String>`, closed set | `extend("enum" = [ …values…, null ])` — **`null` included** |
  | open set (`traces.slow.group_by`) | description only, no `enum` |
- **`traces.slow.group_by` (#1) gets a description, not an `enum`** — its accepted set is
  genuinely open, and declaring `["name"]` would be a false positive on every valid call
  that means "don't group".
- **The 45 tool descriptions are authored in the protocol crate.** They are §0's "product",
  and today they live where a daemon cannot reach them.
- **`xtask`'s hand-maintained type list is replaced or completed**, so a tool cannot have no
  schema. `verify-schema` cannot catch this today: it compares the schema against the list
  it was given, so an omission is invisible from both sides.

**Exit, and it is a query rather than a judgement:** every request parameter with a
closed accepted set declares that set in the schema, alias-complete; every tool in `TOOLS`
resolves to a schema.

**Worth alone:** enum validation for the SDK and every schema consumer, and the
`rename_session` hole closed. Ships without anything below it.

### 11.3.3 Two daemon defects found here, deliberately NOT fixed in Phase A

Both are daemon-behaviour questions surfaced by the grounding pass. Phase A is a
*typing* phase; changing acceptance semantics inside it would be changing the mechanism
when only the description is at fault. They are logged for the daemon session:

1. **`traces.slow.group_by` swallows typos.** `group_by: "nmae"` returns ungrouped spans
   with no error — the caller asked for grouping and silently got none. The comment at
   `rpc_handler.rs:1317` calls this leniency "the documented contract"; the comment at
   `rpc_handler.rs:1757`, about the *same parameter name* on `collectors.get`, calls
   precisely that quiet pass "the failure this whole surface exists to close." **The
   codebase holds two opposite philosophies on one parameter, deliberately, and at most
   one can be right.**
2. **`GroupBy`'s error message understates its own parser.** `rpc_handler.rs:2318` says
   "expected `name`, `group`, `trace` or `path`" while `GroupBy::parse` also accepts
   `none` and `""` (`project.rs:74`). A caller told the truth by the error message cannot
   discover the ungrouped spelling.

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

### 11.4.1 Delivered, 2026-08-01 — and the assertion is clean

`tools.manifest` serves `{protocol_version, broker_version, tools[]}`, each entry carrying
`name`, `method`, `description` and `input_schema`. `TOOLS` became a `Tool` struct holding
the description, and `Tool::definition_name` moved the `collectors.edit` → `CollectorsEdit`
derivation into the protocol crate so both sides resolve parameters by one rule.

**The assertion is clean, and it is four tests rather than one** — each half of a manifest
entry checked against the thing it claims to describe:

| Manifest field | Checked against | Test |
|---|---|---|
| `name` | the router rmcp generates | `the_shared_tool_table_matches_the_router…` |
| `method` | the RPC literal in each tool body | `every_tool_calls_the_method…` |
| `description` | the text rmcp publishes | `every_description_in_the_table…` |
| `input_schema` | the daemon's request type | `param_drift_tests` |

**Descriptions are a deliberate duplicate.** rmcp 1.2 parses `description` as a literal and
rejects a const path — *probed, not assumed*: pointing one tool at a protocol const gives
`error: Unexpected type path`, even though the macro's field is typed `Option<Expr>`. The
attribute therefore cannot reference the protocol crate's copy, and the daemon must serve
text from a crate it links. The duplicate dissolves in Phase C, when the attributes go away.

### 11.4.2 What Phase C actually costs, measured

A generic forwarder can only replace a tool whose body *is* a forward. Counted over
`server.rs`: **36 of 45 are plain forwarders.** Of the nine that are not, **only two carry
genuinely client-side behaviour**:

| Tool | Why it cannot be generic |
|---|---|
| `export_logs` | writes the file itself — a daemon resolves a relative path against its own cwd, not the caller's |
| `get_status` | appends the version-skew note, which is a statement about the shim, not the daemon |

The other seven (`edit_collector`, `document_collectors`, `add_collector`,
`get_log_context`, `snapshot_collector`, `edit_trigger`, `diff_collectors`) are long only
because they build parameters **conditionally** — the very thing daemon-side validation
makes unnecessary, now that absent and null are read as the same thing
(`fix(core): a wrong-typed parameter is an error`). They forward once that conditional
building is dropped.

**So Phase C is: 43 tools become one generic forwarder, 2 keep bespoke handlers.** That the
two exceptions are exactly the two legitimately client-side concerns is the strongest
evidence so far that the split is in the right place.

**Registration happens once, at startup — no `ArcSwap`.** The shim fetches the manifest
before `serve(self)` and builds its router from it. A live-swapping router would only be
needed to gain a tool *without restarting the shim*, and the goal is to gain one without
**reinstalling** it; a restart is not a rebuild. `notifications/tools/list_changed` is the
refinement that would lift that, and it is not required here.

## 11.5 Phase C — the shim registers from the manifest, and D is a separate decision

**C.** `tool_router` becomes a shared handle so registration can happen after `serve()`,
which takes `self` by value today. `#[tool_handler]` generates a **synchronous** `get_tool`
alongside an async `call_tool` returning a `Send` future, so neither `std` nor `tokio`
`RwLock` works — it is `ArcSwap` (**a new dependency**, zero occurrences in `Cargo.lock`) or
three hand-written handler methods. Compiled-in becomes the fallback, fetched the override,
union. The shim is then tool-independent **for MCP**. The three non-passthrough tools
(`export_logs`, `get_status`, `document_collectors`) keep local implementations and are
marked as such in the descriptor (§9.5).

**D — generating the CLI. As specified in §10.2 it is dead; as a phase it is back in scope,
on a different basis** (decided 2026-08-01, because "finish the shim" means the CLI too —
a shim that is daemon-taught for MCP and hand-written for CLI still drags every new daemon
tool back into `crates/mcp`).

The two reasons §10.2 died, and what happened to each:

1. **The derivation table read features this schema does not have.** Measured: `enum` on a
   request parameter — **0**; `"default": true` — **0**; `dependentRequired` — **0**;
   `"type":"boolean"` matches **2 of 16** booleans, because schemars renders `Option<bool>`
   as `["boolean","null"]`. **This is what Phase A is for**, and it is reachable without a
   wire change: `schemars_derive` 1.2.2 supports `extend(...)` (`attr/mod.rs:124`), which
   injects arbitrary schema keywords — so `"default": true` and `dependentRequired` can be
   declared **on the existing types**. `Option<bool>` stays `Option<bool>`, and nothing a
   client sends changes meaning. Phase A is therefore purely additive.
2. **It makes a broker upgrade able to change how the CLI parses argv.** Under §11.0's goal
   this is largely **the intended behaviour** rather than an objection: if the daemon is the
   single point of truth and the point is to enhance it without touching the shim, the CLI
   surface following the daemon is what was asked for. What remains is a consequence to
   manage, not a blocker — and the one real residue is that offline `--help` renders from
   the bundle while execution uses the fetch. Bounded, because a CLI invocation that cannot
   reach the daemon cannot do anything anyway.

**Surface decision, 2026-08-01: take the clean surface.** Command paths are **derived** from
the RPC method (`status.get` → `status get`, `session.list` → `session list`,
`traces.profile` → `traces profile`), with **no `path` override key at all**. Reasoning, and
the second half is the load-bearing one:

- logmon is not deployed, so there are no external scripts to break, and the surface is
  skill-driven — an agent gets the new spelling the moment the skill regenerates.
- **A `path` override table is a place drift can hide again.** A declared path can be
  declared wrong; a derived one cannot. This entire document is a record of declared-vs-actual
  drift, so deriving is not merely simpler, it removes a whole class.

`positional` and `variadic` **stay**, for the two tools that use them — `collectors diff a b`
and `collectors document base after`. Those are argument *shape*, not path, and nothing
derives them; turning them into `--a X --b Y` and a repeated `--name` is the one change that
costs a human at a terminal something real, for no architectural gain.

D still needs **its own design and its own gate** — it does not inherit §10.2's, which the
measurements above killed. It is written after Phase A, against the schema Phase A produces,
which is the mistake §10.2 made in reverse.

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
