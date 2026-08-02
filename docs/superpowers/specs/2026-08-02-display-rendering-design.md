# `_display` — the daemon supplies presentation

Design for gh#10. Tier T2: the code is small, but it mints a wire contract
(who asks, who renders, what the rendered form is), and a contract outlives it.

## 1. What is broken

Since the CLI's hand-written command groups were deleted, **every command prints
raw JSON**, and every MCP tool returns pretty-printed JSON to the agent. The
replacement mechanism — a `_display` string on the result — exists on the client
side (`crates/mcp/src/cli/generic.rs:646`) and has **no producer**.

Grounded at pickup, `2026-08-02`, at `cbef3a4`:

| Claim | Status | Evidence |
|---|---|---|
| The CLI prefers `_display` when present and not `--json` | verified | `crates/mcp/src/cli/generic.rs:646` |
| The MCP forwarding route never reads `_display` | verified | `crates/mcp/src/server.rs:174-189` — `content_field` body, else pretty JSON |
| No daemon code emits `_display` | verified | `git grep _display` → the shim, one doc comment, one unrelated test name |
| The deleted CLI had 20 rendering lines across 9 of 11 groups | verified | `git show 693223d^:crates/mcp/src/cli/*.rs`, per-file `grep -c` |
| 16 of 20 are direct calls to two generic helpers | verified | 10 `format::print_table`, 6 `format::print_blocks` |
| 4 are bespoke functions, covering 3 results | verified | `print_profile`, `print_diff` + `print_diff_rows` (one renderer), `print_query_diagnostics` (a supplement, not a standalone site) |
| `cell()` and `flatten()` already solve the escaping | verified | `crates/core/src/cases/document.rs:242` and `:254` — private, so the reuse in §4.1 is a real move |
| `RpcRequest::new` has 19 call sites in 11 files, incl. the SDK | verified | `grep -rc 'RpcRequest::new' crates/` — `sdk/src/bridge.rs`, `sdk/src/reconnect.rs` among them |
| 50 param structs carry `deny_unknown_fields`, but only **4 of 49** handlers ever deserialize one | verified | `grep -on 'let req: [A-Za-z]*' rpc_handler.rs` → `DomainsCreate`, `DomainsDelete`, `SessionsRename`, `DomainsUse`. The other 45 read fields one at a time (92 `opt_*`/`req_*` calls) |
| The SERVED schema forbids extra properties on 50 of 143 definitions | verified | `protocol-v1.schema.json`, `definitions.LogsRecent.additionalProperties == false` |
| `RpcRequest` does not deny unknown fields | verified | `crates/protocol/src/lib.rs:18-24` — plain `Deserialize` |
| Rendering saves 5–6× over pretty JSON | **measured here** | live broker, `domains.list`: 1306 → 223–271 bytes; `logs.recent` ×25: 25,621 → 10,297 |

**The issue's stated saving is real but misattributed.** Compact JSON alone gets
17% losslessly; the rest comes from a rendering *dropping fields*. That makes
"what does each renderer drop" the load-bearing question, not "render or not" —
see §4.

**The issue's open hypothesis is retired, not built for.** It suggested `_display`
might want a width/format hint because a terminal and an agent want different
renderings. Measured on the same result, the spread between every candidate
format is 68 bytes against a 1,083-byte saving — noise. The choice is readability
alone, and one format serves both. Decided with the user, 2026-08-02.

## 2. The contract — the flag rides on the envelope

**Decision: `RpcRequest` gains `display: bool`, `#[serde(default)]`.**

The alternative — `_display: true` inside `params` — fails, and the way it fails
is the argument. An earlier draft of this section said `deny_unknown_fields`
would reject it uniformly. **That is false**, and checking it is what produced
the real reason: only 4 of 49 handlers deserialize a typed param struct at all.
So the same key behaves three different ways:

| who | what happens to `_display` in `params` |
|---|---|
| an MCP client validating against the advertised `input_schema` | **rejected before the call** — 50 definitions carry `additionalProperties: false` |
| the 4 typed handlers (`domains.create/delete`, `sessions.rename`, `domains.use`) | **hard error** at the daemon |
| the other 45 handlers | **silently ignored** — the call succeeds and returns unrendered JSON |

The third row is the one that decides it. A caller asks for a rendered reply,
gets an unrendered one, and nothing anywhere says why.

Making it legal means adding `_display` to all 50 param structs — and that
publishes it: the manifest would then advertise `_display` as a parameter of
`logs.recent`, of `collectors.add`, of every tool, and the generic CLI would
grow a `--display` flag on every command. **A transport preference would become
part of the public tool surface.**

On the envelope it is one field, in one place, and it stays out of the manifest
entirely — a client asking for a rendered reply is not calling a different tool.

**`RpcRequest::new` keeps its signature and sets `display: false`.** It has 19
call sites in 11 files, two of them in the SDK (`bridge.rs`, `reconnect.rs`), so
a fourth positional argument would be 19 edits to express a default. A
`with_display(bool)` builder is what the two callers that want rendering use.
This is a shared-contract change: the whole workspace compiles before it is done,
not just `protocol`.

**Both directions of version skew degrade correctly, and neither needs a
capability:**

| | new broker | old broker |
|---|---|---|
| **new client** | renders | field ignored (no `deny_unknown_fields` on `RpcRequest`) → JSON |
| **old client** | `default` = false → JSON | JSON |

That is the incremental property the issue asks for, obtained from serde's
defaults rather than from a version check.

**`--json` needs no stripping.** The CLI simply does not set the flag when
`--json` is passed, so a script piping into `jq` gets exactly the result it gets
today — no multi-KB text field to filter out. This is the payoff of opt-in over
always-render, and it is why the SDK default is `false`: an SDK consumer reading
fields never pays for a string nothing reads.

## 3. The render hook — one place, keyed by method

**Decision: a single post-processing step in `RpcHandler::handle`.**

`handle` (`crates/core/src/daemon/rpc_handler.rs:174`) is where every synchronous
result converges — one `match request.method`, then one
`Ok(value) => RpcResponse::success(request.id, value)` at
`rpc_handler.rs:236-239`. `handle_async` delegates to it for everything except
`domains.create`, which returns from its own arm and therefore never renders.
That is correct rather than merely tolerable: `domains.create` is not on the
renderer list in §5 and would get `None` anyway.

```rust
// sketch — the only structural change to dispatch
let result = match request.method.as_str() { /* … unchanged … */ };
match result {
    Ok(mut value) => {
        if request.display {
            if let Some(text) = render::for_method(&request.method, &value) {
                value["_display"] = Value::String(text);
            }
        }
        RpcResponse::success(request.id, value)
    }
    Err(msg) => RpcResponse::error(request.id, -32601, &msg),
}
```

**`for_method` returning `None` is the whole incrementality story.** A method with
no renderer gets no `_display`, and the shim falls back to JSON exactly as it
does today. One result type lands at a time, with no flag day and no coordinated
release.

**It renders from the serialized `Value`, not from the typed result.** The typed
structs are already gone by this point — handlers return `Value` — and re-parsing
into 47 types to render them would reintroduce, inside the daemon, precisely the
per-tool knowledge the manifest work removed from the shim. A renderer reads the
fields it names and skips what is absent, which also means a renderer cannot
crash a call: a missing field renders as blank, never as an error.

**A renderer must never fail a request.** `for_method` returns `Option<String>`
and any panic-adjacent path (unexpected shape, wrong type) yields `None`. A
presentation bug that turned a working call into an error would be strictly worse
than the raw JSON it replaced.

## 4. The renderers — two shapes, and what they drop

**Decision: padded markdown for tabular results; blocks for record streams.**
One rendering for both surfaces (user decision, 2026-08-02, from the worked
example in §1's measurement).

Both live in a new `crates/core/src/render/` module — daemon-side, beside the
handler that calls them, and the only place in the tree that knows what a
`logs.recent` result looks like.

### 4.1 `table(headers, rows)` — 9 standalone reads

Padded markdown: aligned in a terminal like the plain columns the deleted CLI
printed, and native structure for an agent.

```
| name    | src       | logs  | spans | oldest  | newest  | bound |
|---------|-----------|-------|-------|---------|---------|-------|
| default | config    | 302   | 10000 | 5540237 | 5628067 | 5     |
```

~20% over unpadded markdown, against the 5–6× already won by rendering at all.

**Cells are escaped.** `|` and newlines in a value would otherwise break the row
into a shape neither surface reads correctly. `crates/core/src/cases/document.rs`
already has `cell()` and `flatten()` doing exactly this, written for the same
hazard in the case document — **reuse them, do not write a second pair.** Their
home moves to a shared module; the case renderer keeps calling them.

### 4.2 `blocks(lines)` — 6 reads

Logs and spans are not tabular: a message contains `|` and newlines, so a table
would need heavy escaping and still read badly. One line per record:

```
[5628067] 2026-08-02T03:29:02 WARN  startup_sweep error: Reconcile failed
```

**An empty result renders its empty marker, never an empty string.** `(no logs)`
is information; a blank reply is indistinguishable from a broken renderer.

### 4.3 What a rendering drops — the sharp part

The measured saving comes from dropping fields, so each renderer's field list is
a decision, not a formatting detail.

- **`additional_fields` stays in the log block when non-empty**, appended as
  `key=value` pairs on a continuation line. It is where an application's own
  context lives — the emitting module, the source file and line, the service name
  — and dropping it is the one omission that would make a rendered reply *worse*
  evidence than the JSON it replaced.

  Measured on 10 real records from a live broker: pretty JSON 6,377 bytes; a
  rendering that drops the fields 1,879 (**29%**); the shipped rendering that
  keeps them 3,105 (**49%**). So keeping them costs 19 points of the saving and
  the feature still halves the payload. The §1 figure was measured on the lossy
  form; **the shipped saving is smaller, and that is the correct trade.**
- **Diagnostics are never dropped.** `verdict`, `narrowed_by`, `truncated`,
  `evicted_before_window`, `capped` — the fields that say what a result could not
  answer — render below the records. This is what `print_query_diagnostics` did,
  and it is the property the deleted renderers' own doc comment called
  deliberate: *a result that prints only the numbers it has reads as complete when
  it is not.*
- **Identifiers a caller may need to pass back** (`stem`, `paths`, `session_id`)
  render verbatim, unabbreviated.

### 4.4 The three bespoke renderers

`traces.profile`, `collectors.diff` and the logs diagnostics line were narrative,
not tabular. **Port their structure from the deleted originals rather than
re-deriving it** — `git show 693223d^:crates/mcp/src/cli/collectors.rs:773-960`.
They encode judgements that are not obvious from the result shape: `(TRUNCATED)`
beside a sample count, clipped-child-span notes, exact-vs-estimated-vs-sampled
ordering. Re-describing them from the field names is the parallel-invention trap.

## 5. Coverage and order

20 sites. The order is by how often a wrong-looking JSON blob is actually in
someone's way, not by how opinionated the old renderer was:

1. **`logs.recent` / `logs.export` / `spans.export` / `traces.*` record reads** —
   the block renderer plus diagnostics. Highest call volume on both surfaces.
2. **The nine list/table reads** — `domains.list`, `collectors.list`,
   `bookmarks.list`, `filters.list`, `triggers.list`, `sessions.list`,
   `collectors.history`, `traces.recent`, `traces.slow`.

   **`traces.slow` renders two ways in the original** — a table when grouped, a
   block list when not (`traces.rs:198` and `:205`). That branch is a property of
   the result, so the renderer keeps it rather than picking one.
3. **The three bespoke** — `traces.profile`, `collectors.diff`, and
   `collectors.get`.

Anything not listed keeps returning JSON, indefinitely and by design.

## 6. The shim side

**The MCP route must prefer `_display`** (`crates/mcp/src/server.rs:174-189`) —
without this the daemon's string arrives as an extra field *inside* the pretty
envelope, which is more tokens rather than fewer and inverts one of the issue's
two stated benefits. Precedence: `content_field` body (a tool that produces a
document) → `_display` → pretty JSON. `notes(&result)` still appends warnings in
every arm; a rendered body must not swallow a warning.

**The MCP route sets `display: true` always; the CLI sets it unless `--json`.**
A tool whose result an agent needs *structurally* is handled by giving it no
renderer — the escape hatch is free and needs no per-tool flag.

The CLI's `emit()` already does the right thing and does not change.

## 7. Tests

Verification:

- A method with a renderer, `display: true` → `_display` present and correct.
- The same method with `display: false` (and omitted entirely) → **no `_display`
  key at all**, and the rest of the result byte-identical to today's.
- A method with no renderer, `display: true` → no `_display`, no error.
- Table escaping: a value containing `|` and a newline stays one row.
- An empty list renders its marker, not "".
- `additional_fields` reaches the log block; `verdict` / `truncated` reach the
  diagnostics line.
- Round trip through `RpcHandler::handle` on a real result, not a hand-built one.

Adversarial:

- **A renderer handed a result of the wrong shape returns `None`**, and the call
  still succeeds with its JSON. Driven by feeding one method's result to another
  method's renderer.
- **An old client** (`RpcRequest` JSON with no `display` key) parses and gets JSON.
- **A new client's request against a struct that denies unknown fields** still
  parses — i.e. the flag really is on the envelope and never reaches `params`.
- MCP precedence: a tool with BOTH a `content_field` body and a `_display` returns
  the body, and its warnings survive.

Negative controls (one per mechanism):

- Delete the `if request.display` guard → the `display: false` test goes red.
- Hard-wire `for_method` to `None` → the presence tests go red.
- Revert `cell()` to identity → the escaping test goes red.
- Delete the diagnostics append → the `verdict` test goes red.

## 8. Non-goals

- **No width or format hint.** Retired in §1 on measured evidence. Adding a
  parameter later is cheap; removing one from a shipped contract is not.
- **No colour.** A rendered string that an agent receives must not carry ANSI
  escapes, and a per-surface variant is exactly the two-rendering design that was
  rejected.
- **No renderer for every method.** The absent renderer is the design, not a gap.
- **The shim regains no result knowledge.** Its rendering policy stays "print
  `_display` if present" — three lines, no tool names.
- **Notifications are not rendered.** A pushed notification has no `RpcRequest`
  and so no flag to carry; nothing in this design reaches that path.
- **Errors are not rendered.** `RpcResponse::error` carries a message a human
  already reads; a rendered error would be a second presentation contract with
  its own precedence rules.
