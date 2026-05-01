# store-test ↔ logmon broker integration — brainstorm handoff (Spec B)

**Audience:** a fresh Claude session that will brainstorm Spec B with Yuval. This document has zero prior context — read it linearly.

**Status:** ready to brainstorm. Spec A (the substrate this depends on) is mostly shipped; Spec B is the next piece.

---

## TL;DR

The store project (`/Users/yuval/Documents/Projects/Store`) has a custom test harness called **store-test** (`store_test` workspace crate). It runs `.tests` files (a small DSL: `test`, `sequence`, `template`, `ensure { }`, `intent { }`, etc.) that exercise the `store_server` binary in process and assert on outcomes. **Each `test` runs in isolation against its own freshly-cloned store template; tests parallelize via an `after` DAG.**

The logmon project (`/Users/yuval/Documents/Projects/MCPs/logmon-mcp`) is a structured-log + OpenTelemetry-trace collector. It receives GELF + OTLP from `store_server` and `ht_server`, indexes them, and exposes them via an MCP server (and now also via a typed Rust SDK — see "Spec A status" below).

**Spec B's question:** what should it look like for a `.tests` test to assert on logmon-collected log/trace evidence as part of its pass/fail verdict? E.g. "the script ran, no ERROR-level logs landed in logmon during the test window" or "this trace appeared end-to-end in the broker."

The original brainstorm split into Spec A (logmon broker-ification — make logmon a multi-client daemon with a typed SDK so non-MCP consumers can call it programmatically) and Spec B (this doc — store-test using that SDK).

Spec A is the substrate. It is shipped enough to support Spec B work today.

---

## Spec A status (the substrate Spec B builds on)

**Repo:** `/Users/yuval/Documents/Projects/MCPs/logmon-mcp`. Branch: `feat/broker-ification`. Tests passing: 229/229.

### What's landed

The single-crate `logmon-mcp` was converted into a 7-crate workspace:

| Crate | Role |
|---|---|
| `logmon-broker-protocol` | Wire types: `RpcRequest`/`Response`/`Notification`, all 50 typed param/result structs, 14 shared types, `TriggerFiredPayload` notification payload. Schema generated to `crates/protocol/protocol-v1.schema.json` via `cargo xtask gen-schema`. Pre-commit drift guard via `cargo xtask verify-schema`. |
| `logmon-broker-core` | The daemon engine: GELF/OTLP receivers, log/span store, filter/trigger machinery, session registry, RPC handler. Houses `daemon::server::run_daemon`. Has a `test-support` feature that exposes an in-process daemon harness (`TestDaemonHandle`, `TestClient`) for integration tests — **this is the API Spec B will likely consume from store-test.** |
| `logmon-broker` | The daemon binary (`logmon-broker`). Reads `~/.config/logmon/config.json`. clap CLI with `status` subcommand stubbed for Task 18. |
| `logmon-broker-sdk` | Typed Rust SDK consumers use to talk to the broker over JSON-RPC over UDS (or TCP on Windows). `Broker::connect()` builder pattern, broadcast notifications, named-session reconnection wired in `BrokerBuilder`. Full typed dispatch and reconnection state machine pending Tasks 13–17. |
| `logmon-mcp` | The MCP shim. Connects via SDK; surfaces tools to MCP clients (Claude Code etc). |
| `xtask` | `gen-schema` + `verify-schema` subcommands. |

Wire protocol: **JSON-RPC 2.0 over Unix Domain Socket** at `~/.config/logmon/logmon.sock` (or TCP `127.0.0.1:12200` on Windows). `PROTOCOL_VERSION = 1`. `session.start` is the handshake; subsequent calls multiplex on the same connection.

### Three protocol additions in flight

1. **`oneshot: bool` on triggers** (Task 10, ✅ shipped, commit `fd14c72`). A `oneshot` trigger fires once, then auto-removes. Useful for "wait for exactly this event" flows.
2. **`client_info: Option<Value>` on `session.start`** (Task 11, partially shipped — protocol field added in Task 9.5; 4 KB size cap not yet enforced). Lets a connecting session declare e.g. `{"name": "store-test", "version": "0.1.0", "run_id": "..."}` for diagnostic surfaces.
3. **Capability vector on `session.start` response** (Task 12, pending). Daemon returns `["oneshot_triggers", "named_sessions_v2", ...]` so SDKs can feature-gate.

### What's NOT shipped yet

**SDK side (Tasks 13–17 of broker-ification plan):**
- Typed method dispatch wrapping the JSON-RPC raw `call()` — currently `Broker::call(method, params) -> Value`; we want `broker.triggers_add(&TriggersAdd { ... }) -> TriggersAddResult`.
- Notification typed enum on `subscribe_notifications()` — currently `RpcNotification` (untyped); we want `Notification::TriggerFired(TriggerFiredPayload) | Reconnected | SessionLost | ...`.
- Filter builder API — currently filters are passed as raw strings; we want type-safe selectors.
- Reconnection state machine — named-session resume across daemon restart, in-flight RPC behavior, the `Reconnected` notification.
- Notification ordering guarantees across reconnect.

**Service deployment (Tasks 18–25):** broker `status` subcommand, SIGTERM/SIGINT graceful shutdown, sd-notify, stale PID cleanup, shim broker discovery (env/PATH/sibling), launchd plist on macOS, systemd unit on Linux, manual smoke-test checklist.

**For Spec B brainstorming purposes:** none of these are blockers. Store-test could use the **in-process test-support harness** (already shipped in Task 9.5) to spawn its own daemon per test run and call into it via the raw `TestClient` API — that's the cleanest seam right now and avoids the not-yet-shipped reconnection logic and service deployment plumbing entirely. More on that in "Likely architecture" below.

### Where to look

- **Spec doc:** `docs/superpowers/specs/2026-04-30-broker-ification-design.md`
- **Plan doc:** `docs/superpowers/plans/2026-04-30-broker-ification.md` (30 tasks)
- **Test-support harness API:** `crates/core/src/test_support.rs` — `TestDaemonHandle`, `TestClient`. Read this before brainstorming, it's the seam Spec B will most likely use.
- **Protocol shape:** `crates/protocol/src/methods.rs` (param/result structs), `crates/protocol/src/notifications.rs` (`TriggerFiredPayload`), `crates/protocol/protocol-v1.schema.json` (generated).
- **Wire format conversion:** `crates/core/src/daemon/server.rs` `pipeline_event_to_trigger_fired` — engine `PipelineEvent` → wire `TriggerFiredPayload`.

---

## Spec B territory — what to brainstorm

The original Spec A/B brainstorm decomposed the integration goal into concrete subproblems. Spec A handled the substrate. Spec B inherits these open questions:

### 1. Test isolation model

How does a `.tests` test "scope" its observation of logs/traces?

The store-test harness already runs each `test` in its own freshly-cloned store template (commit-level isolation). But logmon collects a global stream — multiple parallel tests would all emit into the same broker. Options:

- **Named session per test.** Each test connects to the broker with `session.start` `name: "store-test/<file>/<testname>/<run_id>"`; sets up its own filters/triggers; tears down at end of test. Works because logmon already supports per-session triggers/filters and persists named sessions.
- **Separate daemon instance per test.** Spawn an in-process broker via `TestDaemonHandle` inside the test fixture. Heaviest isolation, slowest setup, but bullet-proof.
- **Tag-based scoping.** All store_server log records emitted during a test carry e.g. `_test_run_id=<uuid>`; logmon filters can match `e.test_run_id == "..."`. Cheaper than named sessions; doesn't require touching session machinery.
- **Trace-based scoping.** Each test starts a top-level OTel span; everything the test triggers becomes descendant spans. Logmon already correlates logs to traces. The test asserts on `traces.get(trace_id)` rather than on raw logs.

The trace-based option was favoured in the original brainstorm because the store project already runs OTel and logmon already does trace correlation. But it's not the only viable choice; the brainstorm should weigh setup cost vs assertion expressiveness.

### 2. Assertion surface

What does an assertion look like in a `.tests` file?

The store-test DSL today uses Rhai-bodied tests. Examples like `assert(...)`, `run_script(...)`, `assert_via(...)` already exist. A new assertion family would let a test say things like:

- "no ERROR-level logs from this run"
- "exactly one log matching `/Reconcile failed/i`"
- "trace `X` completed end-to-end with no error spans"
- "the slow-trace bucket gained ≥1 entry"
- "trigger `foo` fired exactly once"

Open: should these be Rhai-callable functions (e.g. `assert_no_logs("level >= ERROR")`)? Or a new SDL-like keyword block (`expect_logs { level >= ERROR; count = 0 }`)? Or both?

### 3. SDK seam

Where does store-test consume the broker?

Three options, in order of decreasing coupling:

- **In-process harness via `test-support` feature.** Each `cargo test` run on store-test depends on `logmon-broker-core` with `features = ["test-support"]`, spawns a `TestDaemonHandle`, and store_server is configured to ship GELF+OTLP to the harness's tempdir socket/ports. Pros: zero external state, deterministic. Cons: cargo dep on logmon — logmon becomes a test-time dep of store-test.
- **External broker via SDK.** store-test discovers a running `logmon-broker` (production daemon at `~/.config/logmon/logmon.sock`) via `logmon-broker-sdk::Broker::connect()`. Pros: matches production setup; the developer can `tail` logmon while debugging. Cons: needs the daemon to be running; tests interact with the developer's live log stream; per-test isolation now relies on naming hygiene. Today's broker doesn't yet have the reconnect state machine, so a daemon restart mid-suite would fail tests.
- **Hybrid.** Default to in-process; flip to external via env var (`STORE_TEST_LOGMON=external`) for ad-hoc debugging.

The hybrid is probably right. The original brainstorm leaned this way.

### 4. Performance & parallelism

The store-test suite runs ~205 tests in parallel. How many broker sessions / daemons can run concurrently?

- In-process per-test daemon: 205 × tempdir + UDS bind. Almost certainly fine on macOS/Linux; UDS is cheap.
- Single in-process daemon per `cargo test` invocation, named-session per test: 1 daemon, 205 sessions. Simpler, less isolation. The session registry was designed for this; named sessions support disconnect+reconnect, drop, persistence.
- External daemon, named-session per test: works, but the developer's live broker accumulates 205 dead named sessions per run unless we also wire automatic `session.drop` on test teardown. (Or the harness does the drop.)

### 5. What does store-test need that the broker doesn't have yet?

The brainstorm should produce a list. Plausible items:

- A `logs.recent` filter that scopes to a session ID (currently the daemon stores globally; per-session views are filter-driven). Or maybe what we want is "logs during this connection's lifetime" — that's not a filter, that's a temporal scope.
- A `traces.recent_since(checkpoint)` API so a test can say "anything new from when I started?".
- A "session log scope" mode: tag every log received during this session with the session ID, so filters can narrow.
- A `bookmark` API integration — store-test could push a bookmark at test start/end and use `b>=name AND b<=name` to scope. Bookmarks already exist in the broker; this would be a thin wrapper.
- `oneshot` triggers (✅ shipped) for "wait until this event happens, then continue". Already useful.

### 6. Failure-mode UX

When a test fails because of a broker assertion, what does the developer see?

- The matched log entries inline in the test failure output? (Likely yes, but bounded — the broker can return huge result sets.)
- A link/path to a `logmon`-formatted dump? (Helpful; integrates with the existing `/logmon` MCP workflow.)
- A reproduction command? (`logmon broker filter ... since=<timestamp>`?)
- The trace_id with a click-through?

This is the highest-value question for developer experience and probably underserved if we just bolt SDK calls onto the existing `assert(...)` machinery.

### 7. Scope discipline

The original brainstorm explicitly deferred:
- Driving store_server config from the test (which GELF/OTLP endpoints to ship to). Should test fixtures control this?
- Multi-process tests (e.g. `store_server` + `ht_server` interaction) — out of scope for v1.
- Replaying an external log stream into a test (i.e. canned logs as fixtures).

Brainstorm should restate the scope: minimum viable assertion family + integration; defer everything else with explicit "future work" notes.

---

## Cross-repo references

### logmon-mcp side (`/Users/yuval/Documents/Projects/MCPs/logmon-mcp`)

| Path | Why it matters |
|---|---|
| `crates/core/src/test_support.rs` | The harness API store-test will likely call. `TestDaemonHandle::spawn()`, `inject_log`, `connect_anon`, `connect_named`. |
| `crates/protocol/src/methods.rs` + `notifications.rs` | All RPC types with serde + schemars. |
| `crates/protocol/protocol-v1.schema.json` | Generated JSON Schema (drift-guarded). Useful when explaining the wire shape. |
| `crates/sdk/src/connect.rs` | `Broker::connect()` builder. The "external broker" path uses this. |
| `crates/core/src/daemon/server.rs` | `run_daemon` + `run_with_overrides` entry points. `pipeline_event_to_trigger_fired` for wire format. |
| `crates/core/src/engine/trigger.rs`, `daemon/session.rs`, `daemon/log_processor.rs` | Engine semantics. |
| `docs/superpowers/specs/2026-04-30-broker-ification-design.md` | Spec A. |
| `docs/superpowers/plans/2026-04-30-broker-ification.md` | Plan A (30 tasks; 11 done). |

### Store side (`/Users/yuval/Documents/Projects/Store`)

| Path | Why it matters |
|---|---|
| `store_test/src/test/` | DSL parser + harness. Modifiers: `test`, `sequence`, `template`, `ensure { }`, `intent { }`. Test bodies can be Rhai (`harness script`). |
| `tests/*.tests` | Existing test files. Browse for the conventions in use. |
| `docs/guides/store-test.md`, `store-test-tutorial.md` | DSL reference + tutorial. |
| `store_server/src/` | The system-under-test — emits GELF + OTLP. |
| `tracing_init` | Crate that does GELF + OTLP setup; lives at `../tracing_init` (sibling to Store). |
| `docs/guides/logmon.md` | logmon usage guide; describes the GELF/OTLP shipping contract. |
| `docs/superpowers/specs/2026-04-23-test-suite-foundation-design.md` | Test infrastructure design — context for how store-test came to be. |
| `docs/superpowers/specs/2026-04-22-dsl-harness-wave2b-design.md` | Most recent harness expansion (file access, logmon assertion stubs, Rhai-bodied tests). May already have placeholder hooks for logmon assertions; check before designing in a vacuum. |
| `~/.claude/projects/-Users-yuval-Documents-Projects-Store/memory/MEMORY.md` | Project memory index. Multiple entries about test strategy / intent-driven coverage / harness completeness. Worth a glance. |

### Wave-2b harness file access

Note: Wave 2b shipped `read_file`, `from_toml`, `read_toml`, `read_json`, plus `log_info`/`log_warn`/`log_error`/`log_debug` Rhai functions that emit through `tracing_init` to logmon. **So tests today can already produce logmon log records** from inside the harness (the test logs become broker entries). What's missing is the assertion side — calling _back_ to logmon to read what arrived.

---

## Conventions to know

- **Plan-before-code rule** (CLAUDE.md mandate): for non-trivial changes, brainstorm → spec → plan → review → implement. Don't skip to implementation.
- **Subagent-driven development**: substantial multi-task plans get executed via `superpowers:subagent-driven-development` skill — implementer + spec-reviewer + code-quality-reviewer per task. The broker-ification work used this pattern.
- **First-time correctness**: project policy is "code must be correct, clean, efficient on first write." Iteration speed is NOT a goal. Save implementation time is NOT a goal.
- **Correctness over effort**: prefer architecturally correct option over smaller-effort patch. If a behavior surprises a careful reader, fix it (or document it as "expected surprise").
- **Skills as long-term memory**: when conventions change, update the skill files in the same session.
- **Co-author commits**: `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`.
- **Branch hygiene**: new work goes on a feature branch. Don't commit directly to main without explicit user consent.
- **Push over SSH only**: HTTPS keychain prompt is blocked in these sessions.

---

## Suggested kickoff for the new session

1. Read this document.
2. Skim `crates/core/src/test_support.rs` (the harness API) — it's the most likely seam.
3. Skim `docs/guides/store-test.md` on the Store side — understand the existing assertion surface.
4. Open the brainstorm with Yuval. Suggested opening framing:
   > "I want to design store-test ↔ logmon integration. The substrate (broker SDK + test harness) is real. I see four open axes: **isolation model** (named session vs separate daemon vs trace-scoped), **assertion shape** (Rhai functions vs DSL block), **SDK seam** (in-process vs external vs hybrid), **failure-mode UX** (inline excerpt vs trace link vs reproduction). Where do you want to start?"
5. Output: a Spec B design doc at `/Users/yuval/Documents/Projects/MCPs/logmon-mcp/docs/superpowers/specs/2026-04-30-store-test-integration-design.md` (or wherever Yuval prefers — possibly the Store side).

The brainstorm should NOT skip straight to a plan. Spec first, then plan after Yuval signs off.

---

## Quick status snapshot of broker-ification (so the new session knows the substrate is real)

```
Branch: feat/broker-ification (logmon-mcp)
Tests:  229 passing
Schema: up to date

Phase 1 (workspace conversion):       ✅ 6/6 tasks
Phase 2 (typed protocol + schema):    ✅ 4/4 tasks (incl. Task 9.5 harness)
Phase 3 (additive extensions):        ⏳ 1/3 (oneshot done; client_info partial; capabilities pending)
Phase 4 (SDK polish):                  ⏳ 0/5
Phase 5 (service deployment):          ⏳ 0/8
Phase 6 (docs):                        ⏳ 0/3

Next implementation task:              Task 11 (client_info 4 KB validation)
                                       — pause here for Spec B brainstorm.
```

Spec B work does NOT need to wait for Phases 4/5/6. The in-process harness is the cleanest entry seam and is already shipped.
