# Capability skew, made visible — design

**Status:** draft for design gate. No code written.
**Tier:** T1 — additive fields on an existing handshake plus one new read-only RPC. No contract is minted; no existing shape changes.
**Supersedes:** `2026-07-30-daemon-taught-tools-design.md`, withdrawn after its own gate. §7 records why.
**Seams verified at:** `98e75cc`. Every row below is a command run against this tree, not a recollection.

---

## 0. The incident, stated precisely

On 2026-07-30 the Store project used the collectors for the first time and filed a
report proposing seven improvements. Three of them — labelled arms, compare-two-arms,
carry-provenance — described features that **already shipped**
(`snapshot_collector(label=)`, `collectors.diff`, snapshot `meta`). Their shim binary
was several minor versions behind the daemon, and both the tool list and the skill
file are compiled into that binary.

The precise failure: **the daemon gained capabilities the shim could not express, and
nothing anywhere said so.** Not "the docs were bad" — the docs were fine, and three
versions old.

The narrow fix is therefore not "let the daemon teach the shim new tools." It is
**make the gap visible**, in the two places a reader would meet it.

---

## 1. Seams — verified at `98e75cc`

| Seam | Status |
|---|---|
| A capability channel at handshake | **Already exists.** `SessionStartResult` (`protocol/src/lib.rs:82`) carries `capabilities: Vec<String>`, populated at `daemon/rpc_handler.rs:248-253` with a hardcoded `["bookmarks","oneshot_triggers","client_info","domains"]`. |
| Why the handshake is **not** used, despite having a precedent | `capabilities` shows how to add a field there — `#[serde(default)]`, always serialized (`protocol/src/lib.rs:90-94`). But `parse_session_start_response` (`sdk/src/reconnect.rs:314-325`) is a strict `from_value`, so a field declared without that attribute aborts shim startup against an un-upgraded daemon. An earlier revision of this design did exactly that. Riding `status.get`'s response instead removes the hazard rather than mitigating it, and needs no SDK accessors (§5). |
| `get_status` renders the daemon verbatim — **for every shim that has ever existed** | **Confirmed against history, not just HEAD.** At `mcp/src/server.rs:418-428` today, and byte-identical in shape at `90fe745`, the oldest commit containing the tool: `to_string_pretty(&result)` with no interpretation, only the field renamed `bridge` → `broker`. This is the single load-bearing claim of §3.0 — if any shipped shim had ever parsed the status response into a typed struct, tier A would reach only recent installs. It never did. |
| The unknown-method signal | **Every error shares one code.** `rpc_handler.rs:214` maps *all* `Err` to `-32601`; the unknown-method case is distinguished only by its message, `format!("unknown method: {}", …)` (`:209`). §3.1's drift test must match the message, not the code. Noted because citing the code alone would send an implementer down a path that cannot work. |
| A client-side reader for it | **Already exists.** `Broker::has_capability(&self, name)` (`sdk/src/connect.rs:158`). |
| A test for it | **Already exists.** `crates/core/tests/capabilities.rs`. |
| The daemon's method list | **Exists only as match arms.** 43 arms at `daemon/rpc_handler.rs:157-209`. Enumerating them needs a `const` list plus a drift test; there is no runtime reflection. |
| The shim's tool→method map | **Exists, compiled in.** 42 `#[rmcp::tool]` attributes, each making exactly one `.call(` to a literal method string (`mcp/src/server.rs`). The two sets are set-equal today. |
| The skill file | **Compiled into the shim.** `const SKILL_INSTRUCTIONS: &str = include_str!("../../../skill/logmon.md")` (`mcp/src/server.rs:1407`), served as MCP `instructions` at `:1413`. 39 752 bytes. |
| Startup ordering | **Connect precedes serve.** `main.rs:116` `builder.open().await?` runs before `:119` `mcp_server.serve(...)`, so anything fetched from the daemon is available when `get_info()` builds `InitializeResult`. |
| `instructions` is write-once | **Confirmed.** No update method or notification exists in rmcp 1.2's model. Whatever `get_info()` returns is final for the session. |
| Argument validation | **Lives in the shim, via types.** Each tool takes `Parameters<T>`, so serde rejects a mistyped field. Daemon handlers use `params.get(..).and_then(..).unwrap_or(default)` and silently accept. This design does not touch either. |

### 1.1 What the superseded design got wrong here

The withdrawn spec claimed `instructions` was a free channel (it holds the 40 KB
skill), that 41 of 42 tools were pure passthrough (`export_logs` writes a file with
`std::fs::write`), and that reconnect was "already built" for bootstrap (`initial_connect`
does one `connect()` and returns `Err`, with no retry and no timeout on its handshake
read). All three were one command away. They are corrected above.

---

## 2. Scope

**In:** three additive changes that make skew visible.

**Out, deliberately:** daemon-taught tool registration, CLI generation from schemas,
any change to argument validation, any change to `export_logs`, and any restructuring
of `main.rs`'s startup. §7 explains why each was cut.

---

## 3. The design

### 3.0 The channel that reaches shims already in the field

The first version of this spec put the notice in MCP `instructions`, computed by the
shim. Its own gate killed that: **the shim computes it, so only a shim that already
has the fix can report anything.** Every skew that exists today — including the one in
§0 — stays invisible until after an upgrade nobody has a reason to perform. A
mechanism that only detects the *next* incident is not worth a wire change.

`get_status` is different. It is an opaque passthrough (`mcp/src/server.rs:418-428`):
it calls `status.get` and renders `to_string_pretty(&result)` with no interpretation.
**Any field the daemon adds to that response is rendered verbatim by an unmodified
shim of any version.** That is the only channel in the system that reaches installs
already in the field, and it costs one `json!` entry in `rpc_handler.rs`.

So the design has two tiers, and the cheap one carries the load:

| Tier | Reaches | Precision |
|---|---|---|
| **A. Daemon states its own inventory in `status.get`** | every shim ever built, on a daemon restart | prose the agent checks against its own tool list |
| **B. Shim computes the exact diff** | shims built after this lands | names the missing methods precisely |

Tier A alone would have prevented §0. Tier B is the better experience once deployed.

### 3.1 One list, not two

Earlier revisions had the daemon keep its own `AGENT_METHODS` const beside the dispatch
match, and the shim keep a `TOOL_METHODS` const beside its tools. That is **two lists
describing one fact**, and the last gate found the seam between them: forgetting the
daemon-side one passed every check while both tiers reported all-clear.

The shared `TOOLS` table (§3.3) makes the daemon-side const redundant. If a method is
agent-facing it *has* a tool, so `AGENT_METHODS` was `methods_of(TOOLS)` by
construction — a derived value stored twice. **It is deleted.** The daemon reads `TOOLS`
from `crates/protocol`, which it already depends on (`core/Cargo.toml:11`), and emits
`names_of(TOOLS)`.

Removing a list removes the drift between them. What remains is one list checked against
two realities:

- **Against the daemon:** dispatch every `methods_of(TOOLS)` entry at an isolated
  harness handler (the `harness()` shape used throughout
  `crates/core/tests/collectors_rpc.rs`) and assert none returns the **message**
  `unknown method:` — not the code, which every error shares (§1). A method that fails
  on missing parameters returns a different message and passes, which is what makes this
  safe to run over mutating methods like `logs.clear`: the handler is a throwaway and
  the assertion does not require success.
- **Against the shim:** §3.2's `list_all()` equality.

**`domains.create` is included**, correcting the first draft. It *is* an agent tool
(`create_domain`, `mcp/src/server.rs:1295`), and its sync arm returns a specific
"must be dispatched via handle_async" message rather than `unknown method`, so the
dispatch test passes. `session.start` has no tool and is handled in the connection loop,
never by `rpc_handler`, so it is simply absent from `TOOLS`.

**The residual, and it is a choice rather than a limit.** A match arm added with **no
tool at all** is invisible to the checks above — nothing in `TOOLS` changes, so nothing
notices.

Earlier drafts called this unguardable. That was overclaiming: a `#[test]` in
`crates/core` can `include_str!("rpc_handler.rs")`, extract the `"group.verb" =>`
patterns from the dispatch block (`:157-209` — all single-line string literals today),
and assert set-equality with `methods_of(TOOLS)`. This repo already does source-level
invariants of exactly that shape (`mcp/src/server.rs:1421-1442` asserts on the embedded
skill text). Scope the extraction to the `match request.method.as_str()` block so a
comment cannot false-positive.

**Deliberately not built here**, because a method with no tool is not agent-facing and
therefore not what §0 is about — but recorded as available rather than impossible, so
the choice can be revisited on evidence instead of re-derived.

`capabilities` on the handshake is untouched. It is a coarse feature-flag list with
existing consumers, and nothing here needs to change its shape.

### 3.2 The shim computes the gap

The shim has no tool→method map today — each tool body carries its method as a string
literal. The shared `TOOLS` table (§3.3) becomes that map, in `crates/protocol` where
both crates read it rather than in the shim where only one does.

**Its drift test is total, not a pinned count.** The first draft called this
"not checkable" and settled for pinning a length; that was false. `#[rmcp::tool_router]`
generates `GelfMcpServer::tool_router()` (used at `server.rs:20`), and
`ToolRouter::list_all()` is public (rmcp `router/tool.rs:415`). It is an associated fn —
no broker, no daemon, no async. So:

```
assert_eq!(sorted(names_of(TOOLS)), sorted(names(GelfMcpServer::tool_router().list_all())));
```

`list_all()` sorts by name, so the comparison is order-stable once both sides are
sorted. The generated `tool_router()` is **private** and `crates/mcp` is bin-only with
no `[lib]`, so this test lives in the existing `#[cfg(test)] mod tests` inside
`server.rs` (`:1417`), which can reach it. Tool names in `list_all()` are the fn names —
none of the 42 `#[rmcp::tool]` attributes override `name` — so they are what `TOOLS`
keys on.

That catches a tool added without a `TOOLS` row: the case a pinned count misses, and the
case that would otherwise make §3.4 tell a user a tool they are holding is unreachable.

The shim then diffs, per `get_status` call:

```
missing = broker_tools − names_of(TOOLS)
```

For the Store case, a 0.4.0 shim against a 0.8.0 daemon yields
`{collectors.snapshot, collectors.diff, collectors.document, …}` — mechanically, with
no version table to maintain.

**The false positive this must not produce — and which the first draft manufactured.**
If the daemon advertises a method the shim deliberately does not expose, the diff
reports it as missing capability permanently, and upgrading never clears it. The first
draft created the first instance three sections later: it added a `docs.skill` RPC that
no tool exposes, so a shim and daemon built from *the same commit* would have reported
"1 capability unreachable" to every user, forever. Naming the hazard in prose did not
prevent authoring an instance of it.

**Collapsing the two lists (§3.1) is what closes this, and it closes it structurally
rather than by assertion.** A tool that exposes no method cannot exist in `TOOLS` —
the pair is the unit — so there is no longer a set to fall out of agreement with.

Two earlier attempts are worth recording, because both looked like checks and neither
was one. The first pinned `AGENT_METHODS.len()`, which reads one side of a two-sided
relation and cannot see the other change. The second wrote `AGENT_METHODS − TOOLS == ∅`
to catch a *missing* `AGENT_METHODS` entry — but omitting one makes that set smaller,
which cannot violate a subset. Set equality would have caught it; deleting the second
list means nothing has to.

What remains is a `#[test]`, not a build-time guarantee: `cargo install` runs no tests,
so CI catches drift and an installing user does not. Stated plainly rather than
overclaimed.

With `TOOLS` shared (§3.3), the residual limitation shrinks to its honest core: **a
match arm added with no tool at all** is invisible, which no check inside this design
can see.

`daemon_version: String` is added alongside, so the message can name the fix
("`cargo install --path crates/mcp`") rather than only the symptom.

**No field is added to the handshake at all**, which is how this revision closes a
defect two earlier ones carried. The first draft declared `methods: Vec<String>` and
`daemon_version: String` on `SessionStartResult` and then claimed absent fields would
"read as `None`" — they do not; both fail to deserialize, and
`parse_session_start_response` (`sdk/src/reconnect.rs:314-325`) is a strict
`from_value`, so a new shim would have **aborted at startup** against an un-upgraded
daemon — in precisely the upgrade sequence §3.4's notice tells the user to perform. The
second draft fixed it with `#[serde(default)]`. This one removes the surface: both
facts ride `status.get`'s response, which no strict deserializer is on the path of.

### 3.3 Tier A — the daemon states its inventory, in the agent's own vocabulary

The first revision had the daemon emit **RPC method names** and asked the agent to
compare them against the **tool names** it holds. Those are different vocabularies with
no derivable mapping: `traces.slow` is `get_slow_spans`, `collectors.history` is
`get_collector_history`, `collectors.reset` is `reset_collector` while
`collectors.document` is `document_collectors`. An agent doing that comparison by eye
gets both false negatives (reading `get_collector` as covering `collectors.snapshot`)
and false positives (holding every tool, seeing `traces.slow`, finding no such tool).
That is the design's one claim being false in both directions.

**The fix is a single shared table, and it collapses three findings at once.**
`crates/protocol` gains:

```rust
/// The agent-facing surface: (MCP tool name, RPC method). Read by the daemon to
/// state its inventory and by the shim to diff against what it exposes. It does
/// NOT build the shim's tools — those are `#[rmcp::tool]` attributes; a test
/// pins this list to the router they generate.
pub const TOOLS: &[(&str, &str)] = &[("get_status", "status.get"), …];
```

Lives in a named module (`protocol::mcp_tools`) rather than the crate root, so a
future second front-end does not read it as canonical.

Both crates already depend on it (`mcp/Cargo.toml:12`, and core is the protocol's
sibling). Then `handle_status` emits:

```json
"broker_version": "0.9.0",
"broker_tools": ["get_status", "snapshot_collector", "diff_collectors", …]
```

Now the comparison is name-to-name in one vocabulary: the agent holds a tool list and
the daemon states the tool list this broker supports. A missing entry is legible without
any mapping step. Because `get_status` renders the response verbatim (§3.0), **an agent
on a three-versions-stale shim sees this after a daemon restart and nothing else.**

The shim's own version is deliberately not used: it is not advertised to the client at
all — `ServerInfo::new` populates `Implementation::from_build_env()`, which resolves
`CARGO_PKG_VERSION` *inside rmcp*, so the client is told `rmcp 1.2.0`. `broker_version`
is included for the reinstall message, not for a comparison the agent cannot make.

Still no prose hint and no advice from the daemon: a JSON array of tool names in a tool
result is data, not instruction-shaped text.

**A deleted domain still takes the channel down — accepted, not fixed.**
`handle_status` opens `let d = self.resolve_domain(session_id)?`
(`rpc_handler.rs:750-751`), which errors with *"domain X no longer exists — use_domain
to rebind"*. An earlier draft of this section claimed computing the two facts *above*
that line made the channel unconditional. **Implementation disproved it:** the function
still returns `Err`, so the facts go with it. The values are bound above the call anyway,
which records that they could be served, but nothing serves them today.

Serving a partial status would mean giving `StatusGetResult::store` a `#[serde(default)]`
— letting an absent buffer read as an empty one, the absent-vs-zero conflation this
codebase refuses everywhere else. Weighed against a state that takes `use_domain(x)` then
`delete_domain(x)` to reach, whose error names its own remedy, the field stays strict.
Pinned as a limitation by `crates/core/tests/capability_skew.rs` rather than left to be
rediscovered; if it ever bites, the fix is a partial-status path, not a defaulted field.

**Mirrored on `StatusGetResult` (`protocol/src/methods.rs:1677`), with the CLI printing
`broker_version`.** Not optional politeness: the typed SDK path deserializes into that
struct and re-serializes it, so an unmirrored key is **silently dropped for every typed
caller**. This repo has already paid for exactly this — `methods.rs:432-440` records the
daemon emitting `post_remaining` while the type lacked it, and notes that
`verify-schema` cannot catch it because it checks schema-against-Rust, not
daemon-JSON-against-Rust. Nothing catches it here either. Mirroring also puts the broker
version in front of the human running the upgrade, which is the CLI's whole job here.

**The steady-state cost, priced.** `broker_tools` is ~740 bytes compact and ~850 once
`to_string_pretty` renders it, against a current `status.get` body of the same order —
so tier A roughly **doubles** every status response, forever, carrying no information in
the matched case. Accepted rather than optimised: `get_status` is called rarely and
deliberately, and a shim-side filter that removed the array once tier B had consumed it
would break §6's "every key the daemon sent is unchanged" assertion, which is worth more
than the bytes.

### 3.3.1 Serving the skill file: considered and cut

The first draft added `docs.skill` so the daemon could serve `skill/logmon.md`,
reasoning that the skill is what teaches *which move to make*. Cut, for two independent
reasons found at its gate:

- **~94% of it never arrives.** As surfaced to a model, this server's `instructions` is
  truncated at roughly 2,200 bytes, mid-sentence. `skill/logmon.md` is 39,752 bytes.
  The passages the argument rested on — "`snapshot_collector` — **this is the
  between-runs move**", "`diff_collectors` — **this is the payoff**" — live well past
  the cut. Freshening a document whose delivered prefix is 2 KB buys almost nothing.
- **It manufactured the design's own first false positive** (§3.2): a method no tool
  exposes, reported as missing capability to every matched install.

If skill freshness is worth pursuing it should be argued on its own, against what
actually lands in a client's context — not folded in here.

### 3.4 Tier B — the shim names the gap, in the same response

Tier B **does not touch `instructions`.** The shim adds one key to the `get_status`
result it is already relaying:

```json
"shim_note": "This shim exposes 37 of the 42 tools this broker supports. Missing: snapshot_collector, diff_collectors, document_collectors, get_collector_history, edit_collector. Reinstall with `cargo install --path crates/mcp` and restart this MCP server."
```

**A key, not appended prose.** `get_status` renders `to_string_pretty(&result)`, so
trailing text would leave the payload no longer parseable as JSON — a silent
contract change for any consumer that treats a tool result as structured. The shim
already holds the deserialized `Value`, so inserting a key costs nothing and keeps the
result valid. Insert only when the value is a JSON object; relay unchanged otherwise, so
nobody writes `.unwrap()` on `as_object_mut()`. `shim_note` is reserved — the daemon must
never emit it, and §6's round-trip test would catch a collision.

**Two pure functions, because otherwise this cannot be tested.** In one build the
daemon's `broker_tools` *is* `names_of(TOOLS)` — the same const the shim diffs against —
so an end-to-end harness can only ever produce the equal-sets case. Writing the diff
inline in the relay would ship tier B with its note composed only in the no-op path,
while §4 says the whole risk surface is that note being true. So the design requires:

```rust
fn missing_tools(broker_tools: &[String], local: &[&str]) -> Vec<String>
fn annotate(result: &mut Value, broker_tools: Option<&[String]>, local: &[&str])
```

Every tier-B case in §6 is then a table test over `missing_tools` and `annotate` with
synthetic inputs, and the relay keeps no logic of its own.

The first revision put this at the top of `instructions`. Four independent reasons it
belongs here instead, three of which were defects:

- **`instructions` starts with YAML frontmatter.** `skill/logmon.md` opens `---`, and
  `mcp/src/server.rs:1430` already asserts it does. Prepending puts a blockquote at
  byte 0 of the *delivered* string and breaks that invariant — while the existing test
  keeps passing, because it checks the const rather than `get_info()`'s return.
- **It is the scarcest channel in the system.** Only ~2,200 bytes arrive (§3.3.1), and
  those bytes are the frontmatter and the when-to-reach-for-this prose that makes the
  skill fire at all. Spending ~200 of them displaces the `description:` line.
- **It could never update.** `instructions` is write-once and the handshake answer is
  captured once (`sdk/src/reconnect.rs:311` discards all but `is_new` on reconnect), so
  a daemon upgraded mid-session left the notice stale. Computed per `get_status` call,
  it is never stale.
- **It removes a trust hazard.** §3.3 rules out daemon-authored advice because it lands
  as instruction-shaped text; tier B interpolated daemon-supplied strings into position
  0 of exactly that field. A tool *result* is ordinary data, and this text is
  shim-authored over a comparison the shim performed.

It also needs no SDK surface: the shim reads `broker_tools` out of the response it is
already relaying, rather than requiring new `Broker` accessors for handshake fields.

Nothing is appended when the sets match, so the steady state costs nothing.

---

## 4. Failure modes, enumerated

| Situation | Behaviour |
|---|---|
| Shim predates this change (**every install in the field, including §0's**) | Tier B cannot run — the shim has no diff code. **Tier A still reaches it**: `get_status` renders `broker_version` and `broker_tools` verbatim (§3.0), in tool vocabulary, so the agent compares them against the tool list it is holding. This row is the whole reason tier A exists. |
| Shim carries this change, older than daemon | Tier B appends the missing tool names and the fix command to the same response. |
| Shim newer than daemon | `broker_tools` absent from the response ⇒ no note. Honest: a shim cannot know what an older daemon lacks. |
| Daemon predates `broker_tools` | Field absent ⇒ nothing rendered, nothing appended. Identical to today. |
| Either side upgraded mid-session | Recomputed on the next `get_status` call, because neither tier is captured at the handshake. |
| Daemon unreachable at startup | `main.rs:104,116` `?`-exits, as today. **Unchanged and out of scope** — see §7.4. |
| Sets match | No block emitted. |

**This design contains no refusal** — nothing is withheld, blocked, skipped or
version-gated, and every new call has a working fallback. But it does contain one
**claim**, and a claim can be false in a way a refusal cannot:

> N broker capabilities are not reachable from this shim.

That sentence is wrong if `TOOLS` lists a tool the shim does not actually expose — and
it is wrong in a way the reader cannot fix, since upgrading will not clear it. It is
load-bearing: a reader who follows it reinstalls, sees the notice persist, and
reasonably stops trusting the mechanism.

§3.2's `list_all()` equality is what makes it true — `TOOLS` is checked against the
router the shim actually built, not against another list. §6 therefore tests the
equal-sets case as carefully as the gap case.

The superseded design's problems were concentrated in its refusals. This one's whole
risk surface is that single sentence being true.

---

## 5. Compatibility

`PROTOCOL_VERSION` stays 1: two fields added to an existing response are additive, and
the daemon's version gate refuses only a *newer* protocol, not a newer build. No new
RPC method and no persisted format change, so `FORMAT_VERSION` is untouched.

Two artifacts do move, both mechanical and both gated by existing CI: `StatusGetResult`
gains the mirrored fields (§3.3), which makes `crates/protocol/protocol-v1.schema.json`
stale until `cargo xtask gen-schema` is re-run — `scripts/check-schema-drift.sh` fails
loudly until it is.

**Nothing is added to the handshake.** Both tiers ride `status.get`'s response, so
`SessionStartResult` is not modified and `parse_session_start_response`
(`sdk/src/reconnect.rs:314-325`) — a strict `from_value` that would abort startup on an
unexpected shape — is never on the path. The previous revision put the new fields on the
handshake and had to reason carefully about `#[serde(default)]` to avoid bricking the
shim against an un-upgraded daemon; moving to a tool response removes the hazard rather
than mitigating it.

Old shim + new daemon: the extra JSON keys are rendered verbatim (§3.0) — that is the
mechanism, not a tolerance. New shim + old daemon: `broker_tools` is absent from the
response, the shim emits no note, and behaviour is identical to today.

---

## 6. Test list

Rewritten for the tiers as they now stand. The previous revision's list tested
`docs.skill` and a 2 s timeout, neither of which survives §3.3.1 — and it had **no test
at all** for tier A, the tier §3.0 says carries the load.

**Tier A (the load-bearing one).**
- `status.get`'s response carries `broker_version` and `broker_tools`.
- `broker_tools` equals `names_of(TOOLS)` — not a hand-written literal, or the test
  drifts from the thing it checks.
- **Both survive a deleted domain**: bind a session, delete the domain, call
  `status.get`. The call errors today; the fields must be emitted above
  `resolve_domain` so the channel does not go dark (§3.3).
- **Both survive the typed SDK path**: `StatusGetResult` round-trips them rather than
  dropping them, which is the `post_remaining` incident (`methods.rs:432-440`) recurring
  if unmirrored.
- **The passthrough property itself:** a `get_status` tool call returns the daemon's
  JSON with the new keys intact. This is tier A's entire premise (§3.0) and is the
  cheapest thing here to pin.

**Tier B — table tests over `missing_tools` / `annotate` (§3.4), not end-to-end.** In one
build `broker_tools` equals `names_of(TOOLS)`, so a live harness can only produce the
equal-sets row; every other case needs synthetic input.
- Daemon reporting a superset ⇒ the note names exactly the missing tools, in tool
  vocabulary.
- Sets equal ⇒ **no note**. The everyday case, and the one a false positive ruins.
- `broker_tools` absent (old daemon) ⇒ no note, and specifically not an empty-set note
  claiming full coverage.
- Shim holding a tool the daemon does not list ⇒ no reversed-direction claim.
- Result is not a JSON object ⇒ relayed unchanged, no panic.
- **One end-to-end case**, against `spawn_test_daemon` + a `BrokerBuilder::socket_path`
  client: a real `get_status` round trip carries `broker_version`, `broker_tools`, and
  no note.
- **The result still parses as JSON** with the note present, and every key the daemon
  sent is unchanged — asserted by round-tripping the whole payload, so a future change
  cannot quietly turn a structured result into a text blob.

**Drift.**
- `sorted(names_of(TOOLS))` equals `sorted(list_all())` — fails when a tool is added
  without a `TOOLS` row.
- Every `methods_of(TOOLS)` entry dispatches without the `unknown method:` message —
  `TOOLS` checked against the daemon's real dispatch, not against another list.

**Deliberately untested, and available if wanted:** a match arm added with **no tool at
all** is invisible to all of the above. §3.1 records the source-scan test that would
close it and why it is not built.

---

## 7. What was cut from the superseded design, and why

Its own gate — four lenses — returned ~35 findings. Nearly all traced to one
decision: moving the tool surface to runtime. Each cut below removes a class of
finding rather than patching an instance.

1. **Dynamic tool registration.** Required abandoning `#[rmcp::tool_handler]` (its
   generated `get_tool` is synchronous, so a lock cannot be held across it), a new
   initial-connect retry loop (none exists), and a `Vec<Value>`-then-per-element
   parse to get the promised per-row skip. It also deleted argument validation across
   42 tools, since `new_dyn` takes raw `JsonObject` and rmcp validates nothing against
   `inputSchema`.
2. **CLI generation.** Its mapping table was derived from the hand-written CLI's
   flags rather than from the schemas a generator consumes. Five of seven properties
   on `CollectorsAdd` — the tool its own golden test named — do not match: `level` is
   `["string","null"]`, `group_keys` is `["array","null"]`, `threshold` is
   `anyOf[$ref,null]`. Doing this properly needs a `$ref`/`anyOf`/type-union
   resolution layer that was never budgeted, and it does nothing for §0.
3. **The bespoke descriptor format.** MCP already specifies `_meta` with a key
   grammar; the invented `x-` key is deprecated by RFC 6648 and, because
   `rmcp::model::Tool` has no catch-all field, would have forced the very translation
   layer the section existed to avoid.
4. **Restructuring startup for a zero-tool bootstrap.** The daemon-unreachable path
   exits at `main.rs:104` and `:116` before `serve()`, so the `instructions`
   explanation could never have been delivered. Fixing that is a real improvement and
   a separate change; this design neither needs nor pretends to make it.

A future `tools.manifest` remains possible and is not precluded — but it should be
argued on decoupled release cadence, priced honestly, and gated on its own merits.
It is not what §0 requires.
