# Daemon-served skill — design

**Tier:** T2 — changes the daemon↔shim wire contract (`tools.manifest`).
**Status:** design, pre-implementation.
**Architect outcome:** shape A (skill rides in `tools.manifest`), chosen by the user
2026-08-02. Missing field is **non-fatal** (user decision: nothing is deployed in the
wild, so there is no skew population to serve).

---

## 1. Problem

`skill/logmon.md` is compiled into the MCP shim:

```rust
const SKILL_INSTRUCTIONS: &str = include_str!("../../../skill/logmon.md");
```

It is the last piece of daemon knowledge the shim carries. Every other part of the
shim's surface — the tool list, the parameter schemas, the CLI's verbs — is built at
startup from the broker's `tools.manifest` reply. The document that *describes* that
surface is the one thing that does not travel with it.

### The cost is measured, not hypothetical

On 2026-08-02, commits `86cfee3` and `0d9f7c6` changed **only** `skill/logmon.md`.
They read as docs-only commits. They were not: the installed `logmon-mcp` binary
(built 14:17) was verified by string-extraction to carry the pre-change text, and
serving the corrected guidance required `cargo install --path crates/mcp --locked`
plus a restart of every MCP client. The broker — which the change was *about* —
needed nothing.

The failure mode this produces is worse than the inconvenience. The stale text told
agents that "the tool list is compiled into the shim, so upgrading the broker cannot
make a new tool appear," which `86cfee3` corrects as inverted. A shim serving
confidently wrong architectural guidance is the exact class the daemon-taught-tools
work existed to delete.

### Goal

The document describing the tool surface ships with that surface, so the two cannot
disagree by construction.

---

## 2. Load-bearing claims

| # | Claim | Status | Evidence |
|---|---|---|---|
| C1 | The skill is embedded in the shim at compile time | confirmed | `crates/mcp/src/server.rs:340` |
| C2 | `get_info()` is **synchronous**, so instructions must be in hand before the first client call | confirmed | `crates/mcp/src/server.rs:344` — `fn get_info(&self) -> ServerInfo` |
| C3 | The shim already fetches the manifest at startup and fails without it | confirmed | `crates/mcp/src/server.rs:295–313` (`taught_by`) |
| C4 | `ToolsManifestResult` is **schema-only** — never constructed or deserialized at runtime | confirmed | declared `crates/protocol/src/mcp_tools.rs:652`; sole other use is `crates/xtask/src/main.rs:151` (schema generation) |
| C5 | The daemon hand-builds the manifest reply with `json!`, not from the struct | confirmed | `crates/core/src/daemon/rpc_handler.rs:1121` |
| C6 | The shim deserializes only `reply["tools"]`, ignoring every sibling field | confirmed | `crates/mcp/src/server.rs:310` |
| C7 | `crates/mcp` depends on both `logmon-broker-protocol` and `logmon-broker-core` | confirmed | `crates/mcp/Cargo.toml:12–13` |
| C8 | The shim's **production** code references nothing from `mcp_tools`' manifest machinery — the only `manifest()` call is inside `#[cfg(test)] mod tests` (opens at `crates/mcp/src/cli/mod.rs:203`; call at `:292`) | confirmed | `crates/mcp/src/cli/mod.rs:203,292` |
| C13 | An `include_str!` const in the protocol crate is **absent from the shim binary** when the shim does not reference it | confirmed by probe | `SCHEMA_JSON` (`mcp_tools.rs:35`) is referenced by `manifest()` (`:737`), which the broker calls and the shim does not. `strings` on the installed binaries: text ABSENT from `logmon-mcp`, PRESENT in `logmon-broker` |
| C9 | `mcp_stdio.rs` drives a real shim through a real `initialize`, but discards the result body | confirmed | `crates/mcp/tests/mcp_stdio.rs:68–76` |
| C10 | Two existing unit tests assert on the embedded const | confirmed | `crates/mcp/src/server.rs:536,540` |
| C11 | `schema_matches_daemon.rs` has **no** `tools.manifest` coverage | confirmed | grep for `manifest` in that file returns nothing |
| C12 | The plugin ships the skill as a file, independently of MCP | confirmed | `.claude-plugin/plugin.json` → `"commands": ["./skill/logmon.md"]` |

---

## 3. Chosen shape, and what was rejected

**A — the skill rides in the `tools.manifest` reply.** One additive field on a reply
the shim already fetches. No new method, no extra round trip, and the doc becomes
version-locked to the tool surface it describes.

Rejected, each with its reason:

- **B — a separate `skill.get` RPC.** C2 kills the motivating benefit: because
  `get_info()` is synchronous, the shim must hold the text before the client's first
  call, so it has to fetch and cache at startup regardless. A second RPC buys
  independent versioning that nothing asks for, and adds a second call that can fail
  on its own.
- **C — delete the embedded copy, ship only via the plugin file (C12).** Structurally
  the cleanest — it removes a mechanism instead of moving one — but it serves nothing
  to MCP clients that do not install the plugin (Cursor, a bare `logmon-mcp`). That
  is a real population, so this trades a drift bug for a coverage hole.
- **D — A plus an embedded fallback.** Reintroduces the class the change exists to
  delete: a fallback that is silently stale reads exactly like a current one.

---

## 4. Where the text lives

**Decision: the protocol crate**, beside `manifest()` and `SCHEMA_JSON`.

**The skill will not be in the shim binary.** An earlier draft of this section claimed
that because `crates/mcp` depends on the protocol crate (C7), the bytes land in the
shim regardless of where the const lives. That is false, and C13 is the probe that
settles it: Rust materializes a `const` only at its use sites, so an unreferenced one
contributes nothing to a binary. The control is `SCHEMA_JSON` — an `include_str!` of
the entire ~9000-line protocol schema, in the very crate under discussion. It is
**absent** from `logmon-mcp` and **present** in `logmon-broker`, because `manifest()`
references it and only the broker calls `manifest()` outside tests (C8).

So the choice of crate does not decide whether the shim carries the text; **deleting
the shim's reference does.** Once `SKILL_INSTRUCTIONS` is gone and `get_info()` reads
the wire value, the document is absent from the shim binary by the same mechanism that
already keeps the protocol schema out of it.

That reduces the `crates/broker` alternative to one narrow benefit — making a *future*
mistaken reference impossible to write, rather than merely absent — bought at the cost
of threading the text from the binary crate into the RPC handler in `crates/core`. Not
worth it: T6 checks the property directly on the artifact, which is stronger evidence
than an unreachable name.

The protocol crate is right on the merits, not merely by precedent: the skill describes
the tool surface, `manifest()` *is* that surface, and they should be edited together.

Path from `crates/protocol/src/mcp_tools.rs` is `include_str!("../../../skill/logmon.md")`
— the same depth as the shim's current line, since both files sit at `crates/<x>/src/`.

---

## 5. The wire change — and the trap that must not be stepped in

C4 + C5 + C11 combine into the single most likely way this change ships broken:

> Adding a field to `ToolsManifestResult` changes the **published schema** and does
> **not** change what the daemon sends. The struct is documentation. The wire is a
> `json!` literal. Nothing currently compares them.

A design that says "add `skill` to `ToolsManifestResult`" and stops is the textbook SG
defect — a property asserted in a spec and never checked against code. So the change
is explicitly **three** edits, not one:

1. **`crates/protocol/src/mcp_tools.rs`** — embed the const; add the field to
   `ToolsManifestResult` so the published schema describes it.
2. **`crates/core/src/daemon/rpc_handler.rs:1121`** — add `"skill"` to the `json!`
   literal in `handle_tools_manifest()`. *This is the edit that changes behaviour.*
3. **`crates/mcp/src/server.rs`** — read it, store it, serve it; delete the
   `include_str!`.

### Field shape

```
"skill": <string>          // the document, verbatim
```

A bare string rather than an object. There is exactly one document and no metadata a
client can act on — a hash or a length would be a field nobody reads, and a version
would duplicate `broker_version`, which already answers "which build wrote this."

---

## 6. Shim change

C2 forces the shape: `get_info()` cannot await, so the text is resolved in
`taught_by()` and stored on `GelfMcpServer`.

```
taught_by(broker):
    reply    = broker.call("tools.manifest", {})        // unchanged
    entries  = reply["tools"]  -> Vec<ManifestEntry>    // unchanged
    skill    = reply["skill"].as_str().map(str::to_owned)   // NEW, Option<String>
    ...
    Ok(Self { broker, tool_router, skill })

get_info():
    let info = ServerInfo::new(...);
    match &self.skill {
        Some(s) => info.with_instructions(s.as_str()),   // NOT `s`: the bound is
        None    => info,                                 // `impl Into<String>`, which
    }                                                    // &String does not satisfy
```

**The absent arm is non-fatal, per the user's decision.** Note the asymmetry with the
existing `taught_by` behaviour and why it is correct: an absent *tool list* is fatal
(C3) because a shim with zero tools looks to a client like a server that legitimately
offers none, and clients cache that. An absent *skill* degrades guidance without
misrepresenting the surface — every tool still works. Refusing to start because a
document is missing would be a harsher failure than the payload warrants.

Deliberately **not** logged at `warn`: with nothing deployed in the wild the condition
cannot arise from skew, and a warning for an impossible state trains readers to ignore
warnings. `debug` is enough.

---

## 7. Deletion pass

What the embedded copy did besides the happy path, and where each lands:

| Behaviour | Disposition |
|---|---|
| Serving instructions to MCP clients | **Moves** to the wire value |
| `skill_instructions_is_embedded_and_non_empty` (C10) | **Replaced** — it tests a const that will not exist. Its intent (an empty skill silently removes all guidance) survives as a wire-level test, T1 below |
| Plugin file delivery (C12) | **Untouched** — reads `./skill/logmon.md` from the checkout, a different path entirely |
| The `skill/logmon.md` file itself | **Stays** — it is now the daemon's build input and the plugin's payload |

No other reader exists: `SKILL_INSTRUCTIONS` appears at `server.rs:340,346,536,540`
and nowhere else in the workspace.

---

## 8. Test plan

Each row names the seam it drives and the tool that observes it, because the property
"the shim serves the daemon's document" is invisible to any test that does not cross
the process boundary.

| # | Property | Seam | Tool / level | Catches |
|---|---|---|---|---|
| T1 | The daemon **sends** a non-empty `skill` on the wire | `spawn_test_daemon()` + `call("tools.manifest")` | `crates/core/tests/capability_skew.rs` — live daemon, real RPC | **The §5 trap.** The `json!` omission, which no schema test can see |
| T2 | The published schema declares `skill` | `cargo xtask verify-schema` | `scripts/check-schema-drift.sh` | Struct and committed schema drifting apart. **Valid only because `ToolsManifestResult` is already on xtask's hand-maintained list** (`crates/xtask/src/main.rs:151`) — that list is re-run against itself, so it cannot catch a type omitted from it (`main.rs:141`). Adding a field to a listed type is caught; adding a new type would not be |
| T3 | A real shim serves it as MCP `instructions` | `initialize` handshake, reading `result.instructions` | `crates/mcp/tests/mcp_stdio.rs` (C9 — extend; it already initializes and discards the body) | The whole chain end-to-end: daemon → wire → shim → client |
| T4 | The served text **is the daemon's**, not a local copy | assert a marker string present in `skill/logmon.md` at the current commit | `mcp_stdio.rs` | A reintroduced `include_str!` passing T3 vacuously |
| T5 | An absent `skill` starts the shim and serves no instructions | manifest reply with the field stripped | `crates/mcp` — unit over the resolve step, or stdio against a patched reply | The non-fatal arm (§6) |
| T6 | **The skill text is absent from the shim binary and present in the broker binary** | `strings` over both built artifacts, grepped for a marker from `skill/logmon.md` | acceptance check on the build output, not a unit test | The whole point of the change. Verifies the *goal* rather than the mechanism — and would have falsified this spec's original §4 |

### Negative controls

Per mechanism, each observed to fail before the row counts as tested:

- **T1** — remove `"skill"` from the `json!`; T1 must go red. *This is the control that
  matters*: it reproduces the exact defect §5 describes, at its real site.
- **T3** — return `None` from the skill resolution; T3 must go red while T5 stays green.
- **T4** — point the shim at a hardcoded string instead of the wire value; T4 must go
  red while T3 stays green. If both stay green, T4 is vacuous and T3 is not testing
  provenance.

**T4 earns its row** because T3 alone cannot distinguish "served from the wire" from
"served from a const that happens to hold the same text" — which is precisely the state
being left behind, so it is the state a regression returns to.

---

## 9. Out of scope

- **The `top_n` string-typing observation.** During grounding, an MCP client sent
  `top_n` as `"8"` and the daemon rejected it. The schema declares `integer`
  (`protocol-v1.schema.json:7833`) and the CLI path works, so this is client-side
  serialization, not a logmon defect. Noted, not fixed here.
- **Log aggregation.** Separate feature, separately ranked.
- **Skill discoverability.** `profile_traces` existing and being unknown to both author
  and consumer is a real finding (§1 of the conversation that produced this spec), but
  it is a content question about the document, not about how the document is delivered.

---

## 10. Definition of done

All three narrative artifacts updated **in the same session**, ticked mechanically
against the diff (user-directed 2026-08-02):

| Artifact | What it needs for this change |
|---|---|
| `skill/logmon.md` | The §6 absent-skill arm, and the corrected upgrade instruction: a stale skill is now fixed by rebuilding and restarting the **broker**, not the shim |
| `README.md` | The install ordering consequence — `cargo install --path crates/broker` is what ships a skill change; the shim no longer carries one |
| `docs/medium-article.md` | The architectural close: the shim now holds *nothing* about the daemon — tools, schemas, CLI verbs and now its own documentation all arrive over the wire |

**This change inverts the rebuild rule that the rest of this repo currently lives
under**, so the docs edit is not cosmetic — every existing instruction that says to
reinstall `crates/mcp` after a skill edit becomes wrong the moment this merges. Grep
for those instructions as part of the diff; the deletion pass in §7 covers code, and
this covers prose.
