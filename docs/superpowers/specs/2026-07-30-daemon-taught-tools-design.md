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

**Status of the redesign:** §9 (2026-07-31) answers both objections and is the
live document. This file's §§0–8 describe the withdrawn design.

**Tier:** T2 — mints a wire contract (the descriptor manifest) that outlives the code.
**Seams verified at:** `98e75cc` (§§0–8). §9 has not been re-grounded.

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
| Method↔tool mapping | **Confirmed, ~1:1.** 43 distinct RPC methods dispatched by `rpc_handler.rs`; 42 `#[rmcp::tool]` attributes in `mcp/src/server.rs`. The manifest is nearly mechanical, not a curation problem. *(An earlier claim of 92 methods was wrong — it counted match arms.)* |
| Tool bodies | **Confirmed.** 42 tools, 42 `.call(` sites — exactly one RPC per tool, no orchestration anywhere. 41 return `to_string_pretty(&result)` verbatim. |
| The one exception | **Confirmed and deliberate.** `document_collectors` (`server.rs:1045`) returns the markdown itself, not a JSON envelope, and appends `warnings` / `sidecar_name` as blockquotes — because "a reader that has to unwrap a string field before it can read the markdown will spend a turn doing that, and the warnings are what it would skip". §3.4 preserves this. |
| Param structs | **Confirmed.** 34 `*Params` structs in `mcp/src/server.rs`; **zero** in the protocol crate. This is a relocation, not a de-duplication. |
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

# 9. Redesign — 2026-07-31

**Status:** draft. Not gated. §§0–8 above are the withdrawn design; this section
supersedes them where they conflict.

**The change in one line:** the shim becomes **tool-independent** — a generic MCP
adapter for any daemon that speaks the manifest — rather than logmon's shim with a
daemon-taught extension bolted on.

## 9.1 Validation belongs to the daemon, and always did

The gate withdrew the original because *"it deletes argument validation across all
42 tools."* Both the objection and my own first answer to it assumed the same
thing: that the **shim** is where validation lives. It is not, and it should not
be.

**The socket is open.** Anything can connect to `~/.config/logmon/logmon.sock` —
the CLI, a script, a future tool, another vendor's client. Verified by probe on
2026-07-31: a raw `nc -U` session reached `logs.recent` in two lines. So any check
that exists only in the shim is not a check. It is a **convention that has held
because there has so far been one careful client**, and the daemon has been
trusting it.

That is the wrong shape independently of this design, and it means the objection's
premise — *the shim validates today* — is only accidentally true.

### What the daemon actually checks today

| | Checked? | Evidence |
|---|---|---|
| `level: "tre"` is not a legal level | ✅ errors | seam table above |
| Filter DSL syntax | ✅ errors | `parse_filter` |
| **`count: "abc"` — present but wrong type** | ❌ **silently substitutes `50`** | `rpc_handler.rs:757`, and probed live |
| `before` / `after` wrong type | ❌ silently substitutes `10` | `rpc_handler.rs:841-842` |

Ten sites read parameters as `params.get(k).and_then(|v| v.as_u64()).unwrap_or(d)`.
A present-but-wrong-typed parameter is therefore **indistinguishable from an absent
one** — the absent-vs-zero conflation this codebase refuses everywhere else,
sitting on the wire.

Probed against the running 0.9.0 daemon:

```
→ {"method":"logs.recent","params":{"count":"abc"}}
← {"result":{"count":50, "buffer_total":1103}}      # no error
```

The caller asked for something the daemon could not interpret, and got fifty
entries and no indication. Today the shim's `Params` structs mask this on the MCP
path, which is precisely the accidental protection described above.

### The rule

> **The daemon validates. The shim transmits.** No check that matters may live
> only in the shim, because the daemon does not control who is calling it.

This is stronger than either the withdrawn design or my first redesign, and it is
simpler than both:

- **The shim carries no validator.** No `jsonschema` dependency, no second
  implementation of the rules, nothing to diverge. This deletes what was open
  question #1.

  And it is the only honest option, because **rmcp validates nothing against
  `inputSchema`** — the retro log recorded this as part of the original
  withdrawal. A `new_dyn` route receives a raw `JsonObject`. So "the shim
  validates" was never going to be free; it meant *the shim ships a JSON Schema
  engine*. Stated that way, against a daemon that must validate anyway because
  the socket is open, it is obviously the wrong place for it.
- **The daemon validates with full knowledge of the tool**, not against a
  published schema's approximation of it. A schema can say `"level"` is one of
  three strings; the daemon knows whether that level is legal *for this
  collector's shape*, whether the seq is inside the buffer, whether the domain
  exists. Structural validation is the floor, and the daemon is the only party
  that can go past it.
- **The manifest's schemas describe rather than enforce.** They build the CLI's
  flag surface and tell an MCP client what to send — both are UX. Enforcement is
  elsewhere and is not their job.

### The precondition this creates

Delegating is only safe once the daemon stops silently defaulting, so **fixing
those ten sites is a precondition of this design, not a consequence of it** — and
is worth doing on its own, today, because the defect is live for every non-shim
caller. The rule to apply at each: *absent takes the default; present-but-wrong
is an error naming the parameter and what was expected.*

### What is NOT lost

- **CLI arg checking stays local and free.** Clap built from the schema (§5.1)
  rejects `--count abc` at parse time as an ordinary consequence of being told the
  type. That is clap doing its job, not the shim implementing validation.
- **Error latency.** A round trip over a Unix socket is microseconds. The
  round-trip objection was never quantified; it does not survive being measured.
- **Enum, range and pattern checking.** Today's derived `Option<String>` cannot do
  any of them (`server.rs:285`). A daemon that validates properly does all three,
  for every client.

## 9.2 Why the second objection has a ladder, not an answer

*"It cannot deliver its own bootstrap explanation."* True of §4.1's "expose zero
tools", which is why that was rejected. But bootstrap is not one situation:

| Situation | What happens | Frequency |
|---|---|---|
| Daemon running | fetch the manifest, register | the normal case |
| Daemon not running, **but startable** | **the shim already starts it** (`auto_start.rs:172` spawns the broker), then fetches | common, and already solved |
| Daemon unreachable, manifest cached from a prior connect | register the **cached** manifest; calls fail with "daemon unreachable, start it with `<cmd>`" | rare |
| No daemon, no cache — a truly cold first run | zero tools, plus `instructions` | once, ever, per install |

**One structural precondition the retro log named and §4.1 did not:** `main.rs`
`?`-exits before `serve()`, so today a failed manifest fetch would kill the
process rather than degrade it. Every row of that table below the first is
unreachable until the fetch is moved off the `?` path and its failure becomes a
value rather than a return. That is the first thing to build, and it is why the
original's answer could not run even in principle.

The original design's rejection of a cache — *"a cached manifest advertises tools
that will fail, and can advertise tools a newer daemon has since removed"* — is
true and is the lesser evil. A tool that fails with *"the daemon is not running,
start it with X"* has told the agent exactly what to do. Zero tools tells it
nothing unless the client happens to surface `instructions`, which is per-client
behaviour we do not control. **A stale-but-actionable answer beats a correct
silence**, and a removed tool called against a live daemon fails with a clear
`unknown method` — which the shim then reconciles on the next manifest fetch.

## 9.3 What "tool-independent" means, concretely

The shim compiles in **no tool names, no parameter structs, no skill text, and no
RPC method names.** What it compiles in is the *mechanism*: transport, the MCP
handshake, reconnect, schema validation, CLI construction, rendering.

Three things must therefore move onto the wire, and the third is the one this
redesign adds over the original:

1. **Tool descriptors** — §3.2's shape, unchanged.
2. **Which RPC each maps to** — §3.2's `x-logmon.method`, renamed to something
   daemon-neutral (`x-manifest`), since the whole point is that logmon is one
   instance.
3. **The instructions/skill text.** `SKILL_INSTRUCTIONS` is
   `include_str!("../../../skill/logmon.md")` (`server.rs:1504`) — compiled in,
   with exactly the staleness problem the tool list has. The 0.9.0 commit message
   named both in one breath: *"both the tool list and the skill file are compiled
   into that binary, so a stale shim is silent by construction."* A redesign that
   fixes one and not the other has fixed half a problem.

**What identifies the daemon** then becomes configuration rather than code: the
socket path, the auto-start command, and the server name. `logmon-mcp` becomes a
thin binary that supplies those three constants to a generic core — and anyone
else's daemon can supply its own.

## 9.4 The one thing that stays local, and why that is not a second mechanism

**Rendering.** The CLI's human-readable output — 2689 lines across 7 files, the
markdown passthrough for `document_collectors`, the log tables — cannot come from
a schema, because a schema describes *what a value is*, not *how it should read*.

This is not §9.1's two-mechanisms defect, and the distinction is worth stating
because it is the line the whole design rests on:

> **Reaching a capability is mechanism. Presenting its result is presentation.**
> One mechanism, always. Presentation may be locally overridden, and its absence
> costs nothing but prettiness — `--json` is always correct.

So: every tool is reachable generically. A tool the local binary happens to know
how to render, it renders; anything else prints JSON. A missing renderer degrades
output, never correctness, and never reachability.

## 9.5 What this buys, and what it costs

**Buys.** Upgrading the daemon makes new capabilities reachable with no shim
rebuild — the original goal. Enum, range and pattern checking the CLI has never
had, **for every client rather than for ours**. A skill file that cannot go stale.
A shim reusable against any daemon that speaks the manifest, which is a larger
thing than logmon. And, as a precondition rather than a side effect, a daemon that
no longer silently substitutes a default for a parameter it could not read.

**Costs, stated because the gate will ask.**

- **Clap built at runtime** (`Command::new`/`Arg::new`), not derived. §5.1 already
  worked this out; it comes back into scope here, because a tool-independent shim
  cannot have a derived CLI.
- **`--help` needs the daemon**, or the cache. Today it does not. This is a real
  regression in the cold case and the cache is what bounds it.
- **The error surface moves into the daemon**, and its messages must be at least as
  good as the ones rmcp produces from a Rust type today. Naming the parameter and
  what was expected is the floor; a worse message is a genuine loss and is not
  avoided automatically.
- **Ten handler sites must change before any of this is safe** (§9.1). Small, but
  it gates the rest.

What is **not** a cost, and was in both earlier versions: a JSON Schema validator
dependency in the shim. There is no validator in the shim. There is nothing to
diverge from the daemon, and nothing to keep in step.

## 9.6 Open questions for the gate

1. **Are the daemon's error messages good enough to be the only ones?** Today a
   wrong type fails in rmcp against a Rust type, with a message serde wrote. After
   this it fails in the handler, with a message we write, and the shim adds
   nothing. That is correct and it is also a quality commitment: every one of the
   ten sites needs a message a reader can act on, and "invalid params" is not one.
2. **What is the cache's invalidation rule?** Keyed on daemon version? On a
   manifest hash? Never, until a successful fetch replaces it? §9.2 leans on the
   cache heavily and does not specify it.
3. **Should this ride `status.get` rather than a new `tools.manifest`?**
   `status.get` already carries `broker_tools` and already reaches every shim ever
   built, which is the property 0.9.0 was designed around. Descriptors are much
   larger than a name list, though, and putting them in a status response makes
   every status call pay for them.
4. **Does `document_collectors`' markdown passthrough survive?** §3.4 handled it
   with a `result: text` hint. That hint is presentation, and §9.4 says
   presentation is local — so either the hint is an exception to §9.4, or the
   passthrough becomes a local renderer. They cannot both be true.
5. **Is the generic core a separate crate?** The reuse claim in §9.5 is only real
   if someone else can depend on it without depending on logmon.
