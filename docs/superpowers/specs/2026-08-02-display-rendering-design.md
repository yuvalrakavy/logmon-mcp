# `_display` — the daemon supplies presentation

Design for gh#10. Tier T2: the code is small, but it mints a wire contract
(who asks, who renders, what the rendered form is), and a contract outlives it.

## 0. Revision — what the design gate changed

Two fresh-context lenses (implementer; soundness + skew) over the first draft
returned 26 findings, 8 of them convergent. **That count is a verdict on the
design pass, not on the gate.** The four decisions in §2–§4 survived both lenses
intact — envelope flag, opt-in, daemon-side renderer, `None`-fallback, and a skew
matrix the soundness lens re-derived against `transport.rs` and confirmed. What
failed was every level of detail below them, and three load-bearing numbers.

One finding was a **live bug in shipped code**, fixed in `e5c6263` before this
revision: `cell()` escaped the pipe and not the backslash, so `id\|name` became
`id\\|name` and split the row, sliding every later cell one header left. Reached
by any filter string matching a literal pipe (`m~/id\|name/`).

## 1. What is broken

Since the CLI's hand-written command groups were deleted, **every command prints
raw JSON**, and every MCP tool returns pretty-printed JSON to the agent. The
replacement mechanism — a `_display` string on the result — exists on the client
side (`crates/mcp/src/cli/generic.rs:646`) and has **no producer**.

**Undeployed, so nobody is feeling it yet.** `crates/mcp/src/cli/` holds only
`connect.rs`, `format.rs`, `generic.rs`, `mod.rs` — the ten groups are gone from
master — but the installed binary predates `693223d` and still renders. Both are
labelled 0.9.0, which hides it. The regression lands the moment 0.10.0 ships, so
**this feature blocks that release** (user decision, 2026-08-02): the point is
not to fix a live regression but to avoid shipping one.

Grounded at pickup, `2026-08-02`, at `cbef3a4`:

| Claim | Status | Evidence |
|---|---|---|
| The CLI prefers `_display` when present and not `--json` | verified | `crates/mcp/src/cli/generic.rs:646` |
| The MCP forwarding route never reads `_display` | verified | `crates/mcp/src/server.rs:174-189` — `content_field` body, else pretty JSON |
| No daemon code emits `_display` | verified | `git grep _display` → the shim, one doc comment, one unrelated test name |
| The deleted CLI had **133 output sites** across **11 of 11** groups | verified | `git show 693223d^:…/<f>.rs \| grep -cE 'println!\|print_table\|print_blocks'`, per file |
| Of those, 10 call `print_table` and 6 call `print_blocks`; **5 bespoke sites** in 4 functions | verified | `print_profile` is called TWICE — `collectors.rs:565` (`collectors.get`) and `:767` (`traces.profile`), same `ProfileResult` |
| ~~20 rendering lines across 9 of 11 groups~~ | **RETRACTED** | that query counted calls to two helpers and named `fn print_*`. `status.rs` scored **0** and has **14** output sites — it renders inline. The one group most obviously missed is the one the query was structurally blind to. See §5: the count was never the right question |
| `cell()` and `flatten()` are at `document.rs:242`/`:254`, private | verified | the reuse in §4.1 is a real move. **`cell()` was also wrong** — pipe escaped, backslash not; fixed in `e5c6263` with a regression test |
| `RpcRequest::new` has 19 call sites in 11 files, incl. the SDK | verified | `grep -rc 'RpcRequest::new' crates/` — `sdk/src/bridge.rs`, `sdk/src/reconnect.rs` among them |
| 50 param structs carry `deny_unknown_fields`, but only **4 of 49** handlers ever deserialize one | verified | `grep -on 'let req: [A-Za-z]*' rpc_handler.rs` → `DomainsCreate`, `DomainsDelete`, `SessionsRename`, `DomainsUse`. The other 45 read fields one at a time (92 `opt_*`/`req_*` calls) |
| The SERVED schema forbids extra properties on 50 of 143 definitions | verified | `protocol-v1.schema.json`, `definitions.LogsRecent.additionalProperties == false` |
| `RpcRequest` does not deny unknown fields | verified | `crates/protocol/src/lib.rs:18-24` — plain `Deserialize` |
| Rendering saves **2–6.7×**, depending on how much of the reply is noise | **measured here** | `status.get` 1967 → 293 (6.7×); `domains.list` 1306 → 223–271 (5–5.9×); `logs.recent` ×25 25,621 → 10,297 (2.5×); the SHIPPED log renderer, which keeps `additional_fields`, 6,377 → 3,105 (**2.05×**) |

**"5–6×" was the best case quoted as the rule.** The shipped log renderer is
**2×**, and §4.1 originally spent "the 5–6× already won" to justify padding's 20%
cost. Re-made at the low end: 20% of a 2× saving still leaves 1.7×, and the
alternative is an unaligned table. The argument survives, but it had to be
re-made rather than assumed.

**The saving tracks NOISE, not encoding.** Compact JSON alone gets 17%
losslessly. The 6.7× on `status.get` comes from dropping 47 tool names an MCP
client already holds; the 2× on logs comes from dropping JSON punctuation and
key repetition. That is why §5's coverage rule is about noise rather than about
site counts.

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

### 2.1 The seam is in the SDK, not on `RpcRequest`

**Both gate lenses found this independently, and it is the design's largest
hole.** An earlier draft said a `with_display()` builder on `RpcRequest` serves
"the two callers that want rendering." Neither caller constructs an
`RpcRequest`. The MCP route calls `broker.call(&method, args)`
(`crates/mcp/src/server.rs:154-158`); the CLI calls `broker.call(&m.entry.method, …)`
(`crates/mcp/src/cli/generic.rs:729`). Both land on `Broker::call`
(`crates/sdk/src/connect.rs:176`) → `DaemonBridge::call`
(`crates/sdk/src/bridge.rs:139`) → `RpcRequest::new` at `bridge.rs:145`, three
layers below either caller. Of the 19 constructor sites, **17 are tests or
`session.start`**; `bridge.rs:145` is the only one that carries a tool call.

So the change is **two new public SDK signatures**, and the builder is an
implementation detail behind them:

```rust
// crates/sdk/src/bridge.rs
pub async fn call(&self, method: &str, params: Value) -> Result<Value>          // unchanged, display: false
pub async fn call_rendered(&self, method: &str, params: Value) -> Result<Value> // display: true

// crates/sdk/src/connect.rs — Broker mirrors it
pub async fn call_rendered(&self, method: &str, params: Value) -> Result<Value>
```

`Broker::call_typed` and the **41** per-method wrappers in
`crates/sdk/src/methods.rs` funnel through `call` and therefore **do not change**
— a typed SDK consumer deserializes into a result struct and would discard
`_display` anyway. `tools.manifest` is fetched with plain `call`
(`generic.rs:629-636`): the CLI needs the manifest's data, not a rendering of it.

A shared-contract change: the whole workspace compiles before it is done, and
§7 pins the default.

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
        // `as_object_mut`, NOT `value["_display"] = …`. Index-assign PANICS on a
        // non-object and silently replaces a `Value::Null` result with an
        // object — a result change, not a presentation change. Every arm returns
        // an object today, so the panic is latent; but the promise below is
        // "cannot crash a call", and `handle_async` runs inside the
        // per-connection loop (`daemon/server.rs:985`) with no `catch_unwind`
        // anywhere in the workspace, so a panic kills that client's connection.
        // The 2026-07-30 capability-skew spec made the same call for `shim_note`.
        if request.display {
            if let (Some(obj), Some(text)) = (
                value.as_object_mut(),
                render::for_method(&request.method, &value_snapshot),
            ) {
                obj.insert("_display".into(), Value::String(text));
            }
        }
        RpcResponse::success(request.id, value)
    }
    Err(msg) => RpcResponse::error(request.id, -32601, &msg),
}
```

(The borrow needs care — render from a read of `value` before taking the mutable
borrow, or render first and insert second. The sketch shows intent, not
borrow-checker-final code.)

**`for_method` returning `None` is the whole incrementality story.** A method with
no renderer gets no `_display`, and the shim falls back to JSON exactly as it
does today. One result type lands at a time, with no flag day and no coordinated
release.

**It renders from the serialized `Value` by default, and MAY deserialize where
the original renderer did.** Handlers return `Value`, and reading fields off it
keeps a renderer total. But the three bespoke renderers in §4.4 lean throughout
on typed `Option` fields (`r.exact`, `r.estimated`, `r.sampled`, `s.self_ms`)
and a null-vs-zero distinction the deleted code's own comment calls deliberate.
Those result types are public in `crates/protocol`, which `crates/core` already
depends on, so `serde_json::from_value::<ProfileResult>(v.clone()).ok()?` is
available and satisfies the never-fail rule by construction. **Allowed, and only
for the bespoke three** — it is ~200 lines of difference in effort. The generic
table and block renderers stay on `Value`, because they are the ones that must
work for a result type nobody has thought about yet.

This is not the shim regaining tool knowledge: the knowledge lives daemon-side,
next to the handler that produced the result.

**A renderer must never fail a request.** `for_method` returns `Option<String>`
and any unexpected shape yields `None`. Reads are already safe — `Value`'s
immutable `Index<&str>` yields `Null` for a missing key or a non-object rather
than panicking — but `Option<String>` does not cover three things, so the rule is
stated as a prohibition on the renderer's *body*: **no byte slicing of a string**
(`&msg[..80]` panics on a multibyte boundary), **no `usize` subtraction** without
`saturating_sub` (column-width maths), **no `unwrap`/`expect`**. §4.4 says to
port from the deleted originals, and the originals were written against typed
structs where some of these were safe.

## 4. The renderers — two shapes, and what they drop

**Decision: padded markdown for tabular results; blocks for record streams.**
One rendering for both surfaces (user decision, 2026-08-02, from the worked
example in §1's measurement).

Both live in a new `crates/core/src/render/` module — daemon-side, beside the
handler that calls them, and the only place in the tree that knows what a
`logs.recent` result looks like.

### 4.1 `table` — the API and the algorithm

**Nothing is being ported here.** The deleted `format::print_table` was
`comfy_table` with `UTF8_FULL_CONDENSED` and `ContentArrangement::Dynamic` — box
drawing and automatic wrapping to the terminal width. There is no padded-markdown
implementation in the tree to copy, so the algorithm is specified rather than
referenced.

```rust
pub fn table(headers: &[&str], rows: &[Vec<String>], empty: &str) -> String;
```

- **Zero rows → `empty.to_string()`**, e.g. `(no domains)`. The marker is a
  parameter because every deleted site had its own string, and a blank reply is
  indistinguishable from a broken renderer.
- **Width of column `i` = `max(display_width(header_i), display_width(cell_i))`
  over all rows**, computed on the ESCAPED cell (what is printed is what is
  measured).
- **All columns left-aligned**, including numeric. Markdown's `|---:|` alignment
  marker is deliberately unused: a right-aligned column requires knowing which
  fields are numeric, which is per-tool knowledge in a generic helper.
- **Separator row is dashes padded to the column width**, matching the example.
- **No truncation and no wrapping.** A 200-character filter DSL string makes one
  long row. `comfy_table` wrapped it; markdown cannot, and wrapping would break
  the row into invalid table syntax. This is a real loss against the deleted
  behaviour and is accepted as the cost of the one-format decision.
- **Display width, not `char` count** — `unicode_width::UnicodeWidthStr`. A CJK
  service name or an emoji in a `facility` misaligns every row below it under
  naive `{:<width$}` padding.

```
| name    | src       | logs  | spans | oldest  | newest  | bound |
|---------|-----------|-------|-------|---------|---------|-------|
| default | config    | 302   | 10000 | 5540237 | 5628067 | 5     |
```

**Cells go through `cell()`** (`crates/core/src/cases/document.rs:254`), which
escapes backslash-then-pipe and flattens newlines. It moves to
`crates/core/src/render/escape.rs`; `cases::document` imports it from there, so
there is one implementation and the case document inherits any future fix.
Note `flatten()` also **trims** — correct for a table cell, and see §4.2 for why
a log message needs the flatten without the trim.

### 4.2 `blocks` — the API, the record line, and the cap

```rust
pub fn blocks(lines: Vec<String>, empty: &str, more_hint: &str) -> String;
```

Joined with `\n`. Zero lines → `empty`.

**It truncates, and says so.** The deleted `print_blocks` capped at
`MAX_BLOCK_RECORDS = 50` and `MAX_BLOCK_BYTES = 16 KiB`, appending
`… N more record(s), …`. Dropping that cap is not an option: `logs.export`
defaults `count` to `usize::MAX` (`rpc_handler.rs:940`), so an unbounded export
would render every stored record into a single `_display` string and ship it —
inverting the entire token argument. **And an uncapped-but-truncated render that
says nothing is the §4.3 failure**: 50 of 1,000 reading as the whole answer.

The hint's wording changes, because the daemon is emitting it now and
`use --json` is shim vocabulary the daemon must not speak: **`… N more record(s)
— narrow the filter or lower `count` to see them`**.

The §1 measurement was taken at 25 records — **below this cap and below
`logs.recent`'s own default of 50** — so it is not a sample of the default call.
§7 adds a case above the cap.

**The record line**, reconciled against the deleted `format_entry`
(`693223d^:crates/mcp/src/cli/logs.rs:170-204`):

```
[5628067] 2026-08-02T03:29:02Z WARN  checkout worker stalled awaiting lock
    app=store  file=src/main.rs  line=250  trace=7f3a…
```

- `[{seq}]`, then RFC3339 to seconds, then the level **upper-cased and padded to
  5**. `Level` serializes as the variant name — `"Warn"`, `"Info"`
  (`crates/protocol/src/methods.rs:22-25`) — so uppercasing is an explicit step,
  not something that falls out.
- The message goes through `flatten()` **without the trim's effect mattering** —
  a newline inside a message would otherwise break the one-line-per-record shape
  the whole block form depends on.
- The continuation line carries `additional_fields` (§4.3) plus `file`, `trace`,
  `span` where present — one continuation line, not two. **Keys sorted**:
  `additional_fields` is a `HashMap<String, Value>` (`gelf/message.rs:101`), so
  unsorted output differs between renderings of identical data and no golden test
  can pin it. A non-scalar value renders as compact JSON.

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
- **The never-drop rule is STRUCTURAL, not a list.** A renderer drops **only the
  record array** — `logs`, `spans`, `rows`, `collectors` — and renders **every
  other key on the result**. My first draft enumerated five field names in prose
  and got it wrong in both directions: three of the five (`verdict`,
  `narrowed_by`, `capped`) are not on `LogsRecentResult` at all, while the two
  that matter most there were missing.

  A list in prose cannot be checked; this rule can, and
  `crates/core/tests/capability_skew.rs::the_typed_status_struct_drops_no_key_the_daemon_sends`
  already implements exactly this key-set diff for another purpose — reuse its
  shape against `_display`.

  What the prose list omitted, and why each matters **to an agent**:

  - **`scanned`** (`methods.rs:249`). `logs.recent` with a filter matching
    nothing over a live buffer returns `{logs: [], count: 0, scanned: 4000}`.
    Rendering only `(no logs)` tells an agent the system is quiet when 4,000
    records just flowed past. The deleted renderer said *"filter matched 0 of
    4000 scanned records — data is flowing, so check the filter"*, computed from
    `empty && scanned > 0` — a **derived** note, not a field, and it must be
    ported as logic.
  - **`cursor_advanced_to`** (`methods.rs:267`). The call MUTATED session read
    position. An agent that is not told will be baffled when the next call
    returns nothing.
  - **`warnings`** — on `CollectorsAddResult`, `CollectorsEditResult`,
    `CollectorsDocumentResult` and `ProfileResult` (`methods.rs:1189, 1384, 1755,
    1849`). `traces.profile` on a collector whose ingest shed spans returns
    percentiles **plus a caveat**; rendering the numbers without it is the
    over-claiming failure this project just spent a feature cycle on. See §6 —
    both surfaces have a live hole here.

  **Diagnostics move from stderr to stdout.** `print_query_diagnostics` wrote to
  stderr so piped stdout stayed clean (`logs.rs:206`). Inside `_display` they are
  stdout. Acceptable under opt-in — a script wanting clean output passes `--json`
  and gets no `_display` at all — but it is a change, and it is stated rather
  than discovered.

  The original rationale, unchanged: this is the property the deleted renderers'
  own doc comment called
  deliberate: *a result that prints only the numbers it has reads as complete when
  it is not.*
- **Identifiers a caller may need to pass back** (`stem`, `paths`, `session_id`)
  render verbatim, unabbreviated.

### 4.4 The bespoke renderers — three functions, FOUR methods

`traces.profile`, `collectors.get`, `collectors.diff`. An earlier draft named
"three bespoke" in §4.4 and a **different** three in §5, which an implementer
would have had to guess between.

The fact that makes it cheap: **`collectors.get` and `traces.profile` already
share one renderer.** `print_profile(&result)` is called at
`693223d^:crates/mcp/src/cli/collectors.rs:565` (`Get`) and `:767` (`Profile`),
on the same `ProfileResult`. Two methods, one renderer.

`print_diff` + `print_diff_rows` (`:893`, `:958`) are one renderer for
`collectors.diff`. The logs diagnostics are **not** a fourth renderer — they are
a supplement appended to the block form, called from two sites (`logs.rs:74`,
`:116`), and they belong to §4.3's structural rule.

**Port their structure from the deleted originals** — they encode judgements not
obvious from the result shape: `(TRUNCATED)` beside a sample count,
clipped-child-span notes, exact-vs-estimated-vs-sampled ordering. Re-describing
them from field names is the parallel-invention trap.

**One thing NOT to port:** `print_profile` does `e.avg_ms.unwrap_or(0.0)`, the
null-rendered-as-zero that the same file warns against elsewhere. An absent
average renders as `—`.

## 5. Coverage — the rule, not a site count

**The primary client is an AI agent; the CLI human is secondary** (user
direction, 2026-08-02). Deriving from that changes what the line is, and
dissolves the site-count question that produced it.

For an agent, **a small flat JSON object is already the ideal reply** —
unambiguous, machine-parseable, and cheap. What costs an agent is *volume and
noise*, not JSON. So:

> **Render where rendering removes noise. Leave a small flat result as JSON.**

Measured against real replies, that rule sorts itself:

| reply | what the agent needs | rendering |
|---|---|---|
| `status.get`, 1967 B | store stats, skew — but **47 tool names are noise an MCP client already holds** | **6.7×** — render |
| record reads (logs, spans, traces) | the records, dense, plus what is missing | **~2×**, no JSON punctuation to mis-parse — render |
| list reads | one row per thing, keys stated once not per-record | **5–6×** — render |
| `filters.add` → `{"id":3,"filter":"l>=ERROR"}` | did it work; the id, to reference later | **~20 B.** Rendering risks hiding a field — **leave as JSON** |

The human check passes on the same line: nobody minds
`{"id": 3, "filter": "l>=ERROR"}`; everybody minds 1,967 bytes of status.

**This is why the 20-vs-133 count was the wrong question.** Most of the 133
`println!` sites were mutation confirmations and decorative headers — exactly the
class the rule leaves as JSON. The coverage barely moves; only the *justification*
does, from "these are the ones with old renderers" to "these are the ones where
JSON costs the agent something."

Order, by how much noise each removes:

1. **Record reads** — `logs.recent`, `logs.context`, `logs.export`,
   `spans.export`, `spans.context`, `traces.get`, `traces.logs`. Block renderer
   plus §4.3's diagnostics. (`logs.context` is one of the six `print_blocks`
   sites and was missing from the first draft.)
2. **`status.get`** — biggest single win, and see §8 for the one part of it that
   cannot move daemon-side.
3. **The nine list/table reads** — `domains.list`, `collectors.list`,
   `bookmarks.list`, `filters.list`, `triggers.list`, `sessions.list`,
   `collectors.history`, `traces.recent`, `traces.slow`, plus `traces.summary`
   (narrative in the original: root span, per-span ms, `(other)`).

   **`traces.slow` has THREE branches**, not two: a table when `groups` is
   present (`traces.rs:198`), blocks when `spans` is (`:205`), and
   `(empty result)` when neither (`:207`). Collapsing the third into
   `(no slow spans)` would claim the query found no slow spans when in fact the
   result carried neither array — a different fact.
4. **The bespoke** — `traces.profile` + `collectors.get` (one renderer),
   `collectors.diff`.

**Mutations return JSON, by rule rather than by omission** — `filters.add`,
`triggers.add`, `logs.clear`, `domains.delete`, `sessions.drop`,
`bookmarks.add`, and the rest. Small, flat, already ideal for the primary client.
§8 records this as a decision.

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

### 6.1 `emit()` DOES change, and the collision is on a tier-1 method

An earlier draft said "the CLI's `emit()` already does the right thing and does
not change." **False, and false exactly where it matters.**

`generic.rs:758-778`: when a tool declares a `content_field` and no `--path` was
given, the CLI prints `reply[content_field]` and **returns** — `emit(&reply, false)`
at `:779` is never reached. Worse, at `:772` `emit` is handed `v`, the content
field's *value*, so it looks for `_display` on the wrong node. `export_logs`
declares `file_output { content_field: Some("logs") }`
(`crates/protocol/src/mcp_tools.rs:205-220`), and §5 puts `logs.export` in tier 1.
So `logmon-mcp logs export` would print a pretty JSON array and never see the
rendering.

**Resolution: `_display` outranks a non-string `content_field`.** A
`content_field` exists so a tool producing a *document* hands back the document
instead of an envelope (`document_collectors` → markdown). Where the field holds
a **string**, that intent stands and the body wins. Where it holds an array —
`export_logs`' `logs` — the "document" is a JSON array nobody wants to read, and
`_display` is strictly better. Precedence, both surfaces:

1. a `--path`/`path` was given → write the file, print the confirmation
2. `content_field` present **and its value is a string** → print the body
3. `_display` present → print it
4. pretty JSON

### 6.2 Warnings, per arm

The first draft said "`notes(&result)` still appends warnings in every arm."
**It does not append in any JSON arm today**, and both surfaces have a hole:

- MCP: `notes()` is called at `server.rs:185` (body arm) and `:205` (file arm),
  **not** in the pretty-JSON arm at `:186-187` — safe only because the JSON
  already contains `warnings`. A new `_display` arm inherits the hole and drops
  them.
- CLI: `print_notes(&reply)` is called at `generic.rs:775` and `:786`, never
  after the plain `emit(&reply, false)` at `:779`.

So: **append `notes()`/`print_notes()` in the new `_display` arm, and nowhere
else.** Adding it to the JSON arm would print each warning twice. Both helpers
carry doc comments saying this repo has shipped the regression before.

### 6.3 Do not render what will be written to a file

The MCP route sets `display: true` unconditionally, but for the two `file_output`
tools (`export_logs`, `document_collectors`, `mcp_tools.rs:211-217, 391-399`) the
shim writes the content field to disk and never reads `_display`. Rendering the
largest result in the system into a string that is transmitted and discarded on
every call is pure waste. **Set `display` only when no output path was resolved**
(`out_path.is_none()`), on both surfaces.

Latent hazard worth naming: `files_to_write`'s `None => render(reply)` branch
(`mcp_tools.rs:705`) would write `_display` **into the user's export file**. Both
live entries set `content_field`, so it cannot fire today — but a future
`file_output` entry without one would corrupt an export.

## 7. Tests

Verification:

- A method with a renderer, `display: true` → `_display` present and correct.
- The same method with `display: false` (and omitted entirely) → **no `_display`
  key at all**, and the rest of the result byte-identical to today's.
- A method with no renderer, `display: true` → no `_display`, no error.
- Table escaping: a value containing `|` and a newline stays one row.
- An empty list renders its marker, not "".
- `additional_fields` reaches the log block, keys sorted.
- **The structural drop rule**: for each rendered method, diff the result's key
  set against what the rendering mentions; only the record array may be absent.
  One test over all renderers, not one assertion per field — modelled on
  `capability_skew.rs::the_typed_status_struct_drops_no_key_the_daemon_sends`.
- Round trip through `RpcHandler::handle` on a real result, not a hand-built one.
  **The six in-repo `h.call(…)` helpers and `ClientHarness::call`
  (`test_support.rs:587`) all build requests with `display: false`** — they gain
  a `call_rendered` sibling rather than a parameter, so existing tests keep
  asserting the unrendered shape.
- **SDK default**: `Broker::call` sets `display: false` and `call_rendered` sets
  true. Pinned directly, because everything else rests on it.

Adversarial:

- **A renderer handed a result of the wrong shape returns `None`**, and the call
  still succeeds with its JSON. Driven by feeding one method's result to another
  method's renderer.
- **A non-object result does not panic.** Construct one and drive `handle`'s
  post-step — the guard in §3, not the `Option`, is what covers this.
- **An old client** (`RpcRequest` JSON with no `display` key) parses and gets JSON.
- **The flag never reaches `params`** — asserted against one of the FOUR typed
  handlers (`domains.create`, `domains.delete`, `sessions.rename`,
  `domains.use`). Against any of the other 45 this passes vacuously, because they
  read fields one at a time and would ignore an extra key.
- **`logs.export` with a `content_field` array and a `_display`** returns the
  rendering, not the array (§6.1) — and with `--path`, writes the file and does
  not request rendering at all (§6.3).
- **Warnings survive**: a `traces.profile` reply carrying `warnings` shows the
  caveat in the rendered output, and shows it **once** (§6.2).
- **Above the block cap**: `logs.recent --count 200` renders 50 records plus the
  more-hint, and the hint states the true remainder.

Negative controls (one per mechanism):

- Delete the `if request.display` guard → the `display: false` test goes red.
- Hard-wire `for_method` to `None` → the presence tests go red.
- Revert `cell()` to escaping the pipe only → the backslash test goes red
  (already in place, `document/tests.rs`, from `e5c6263`).
- Delete the diagnostics append → the structural drop-rule test goes red.
- Remove the block cap → the `count 200` test goes red.
- Restore `content_field` precedence over `_display` → the `logs.export` test
  goes red.

## 8. Non-goals

- **No width or format hint.** Retired in §1 on measured evidence. Adding a
  parameter later is cheap; removing one from a shipped contract is not.
- **No colour.** A rendered string that an agent receives must not carry ANSI
  escapes, and a per-surface variant is exactly the two-rendering design that was
  rejected.
- **Mutations return JSON** — `filters.add`, `triggers.add`, `logs.clear`,
  `domains.delete`, `sessions.drop`, `bookmarks.*`, and the rest of the ~15
  confirmation sites the deleted CLI printed. Not an oversight: §5's rule says a
  small flat object is already the ideal reply for the primary client, and the
  human check passes on the same line. **Recorded here so the next reader sees a
  decision rather than a gap.**

- **`status.get` renders daemon-side except for one line: the CLIENT's own
  version.** The deleted headline was `broker: 0.9.0, cli 0.9.0` — a comparison,
  and the daemon cannot know what version the shim is. So the shim **appends**
  that line to the daemon's rendering. It is the one deliberate exception to "the
  shim regains no result knowledge", and it is not result knowledge but
  self-knowledge: the shim reporting `env!("CARGO_PKG_VERSION")`.

  **The soundness lens called the larger version of this a pre-existing
  regression. It is not** — and `git log` is what shows the difference.
  `skew_note`/`missing_tools` compared a broker's advertised tools against the
  shim's COMPILED-IN list, and were removed deliberately on 2026-08-01 with the
  reasoning left in place (`crates/protocol/src/mcp_tools.rs:548`): a shim now
  registers whatever the broker declares, so **the shortfall they reported cannot
  occur — there is no compiled-in list to fall behind.** The concept dissolved
  rather than moving. Nothing to file; the exception above is the small residue
  that survives, not a lost capability.

- **No renderer for every method.** The absent renderer is the design, not a gap.

- **No terminal-width wrapping.** `comfy_table` did it; padded markdown cannot
  without producing invalid table syntax. A long DSL filter makes a long row.
  This is a real regression against the deleted behaviour, accepted as the cost
  of the one-format decision (§4.1).
- **The shim regains no result knowledge.** Its rendering policy stays "print
  `_display` if present" — three lines, no tool names.
- **Notifications are not rendered.** A pushed notification has no `RpcRequest`
  and so no flag to carry; nothing in this design reaches that path.
- **Errors are not rendered.** `RpcResponse::error` carries a message a human
  already reads; a rendered error would be a second presentation contract with
  its own precedence rules.
