# Logmon Broker-ification Design

**Status:** Spec
**Date:** 2026-04-30
**Author:** yuval@villarakavy.com
**Track:** infrastructure / refactor + extension

## Problem and motivation

`logmon-mcp` today is a single-binary, two-mode (daemon + MCP shim) system whose internal JSON-RPC protocol over Unix domain socket is private. The MCP shim is the only first-class client.

Multiple new first-class clients are coming. The first is `store-test` (a separate spec — Spec B), which needs to assert on logs and traces emitted during DSL test execution. Future clients include `log-store` (long-term log archiving) and possibly dashboards / IDE plugins.

Adding new clients responsibly requires:

- A **stable, documented public protocol** so cross-language clients can codegen and so future versions can evolve without breaking consumers.
- A **typed Rust SDK** so client crates do not reinvent JSON-RPC plumbing.
- A **clean binary boundary** between the long-lived broker daemon and short-lived shim/clients — the role-by-flag pattern is awkward to deploy.
- A **real system-service deployment** (launchd / systemd) so the broker is always-on, not started on demand by the first MCP shim.

This spec covers all four. It is purely a refactor plus minor extension of existing functionality. No new ingestion paths, no new query semantics, no breaking changes.

## Goals and non-goals

### In scope

- Convert the single-crate repo to a Cargo workspace with five member crates: `logmon-broker-protocol`, `logmon-broker-core`, `logmon-broker`, `logmon-broker-sdk`, `logmon-mcp`.
- Split the binary into `logmon-broker` (daemon) and `logmon-mcp` (MCP adapter).
- Freeze the JSON-RPC method surface as `PROTOCOL_VERSION = 1` with three additive extensions: oneshot triggers, optional `client_info`, response `capabilities`.
- Emit `protocol-v1.schema.json` from `schemars` derives at build time, committed to the repo.
- Provide a typed Rust SDK with: handshake, every RPC as a typed method, a typed filter builder, broadcast notification subscription, and named-session reconnection with exponential backoff.
- Add `logmon-broker install-service` / `uninstall-service` / `status` subcommands.
- Add systemd `Type=notify` `READY=1` signaling on broker startup.
- Preserve the OTel UDP-multicast beacon (`OTEL:ONLINE` / `OTEL:OFFLINE`) unchanged.

### Out of scope

- No protocol-shape migration. JSON-RPC over UDS stays.
- No new ingestion path. GELF + OTLP receivers unchanged.
- No MCP tool surface changes. All 27 tools and their parameters remain identical.
- No project rename on disk. `logmon-mcp` repository keeps its name.
- No store-test integration. That is Spec B.
- No Windows system-service support. Windows keeps shim auto-start as today.
- No live-tail subscription primitive. Flagged as future direction.
- No SDK `MockBroker`. YAGNI per discussion; can be added when first consumer needs it.
- No tooling-compat shim for the deprecated `logmon-mcp-server` binary name.

## Architecture

### Workspace and crate layout

| Crate | Visibility | Depends on | Purpose |
|---|---|---|---|
| `logmon-broker-protocol` | **public** | (none) | JSON-RPC types, `PROTOCOL_VERSION`, `schemars`-derived JSON Schema export. Tiny; types only. |
| `logmon-broker-core` | private | protocol | Engine: pipeline, filter parser/matcher, trigger manager, span/log stores, bookmark store, session registry, persistence, RPC handler. Daemon-only library. |
| `logmon-broker` | binary | protocol, core | Daemon binary. Owns connection accept loop, lifecycle, OTel beacon, systemd ready notification, install-service subcommand. |
| `logmon-broker-sdk` | **public** | protocol | Typed Rust client SDK: connect, handshake, method dispatch, filter builder, broadcast notifications, reconnection. No MCP knowledge. |
| `logmon-mcp` | binary | protocol, sdk | MCP adapter binary. Auto-starts `logmon-broker` if no daemon running. Maps MCP tools 1:1 to SDK calls. |

Two binaries: `logmon-broker` and `logmon-mcp`. The role-by-flag pattern (`Cli::Daemon` vs no subcommand) goes away.

### Source-tree mapping

| Current path | New path |
|---|---|
| `src/rpc/types.rs` | `crates/protocol/src/lib.rs` (with `schemars` derives + schema export) |
| `src/{engine, filter, gelf, receiver, span, store}` | `crates/core/src/{engine, filter, gelf, receiver, span, store}` |
| `src/daemon/{log_processor, span_processor, persistence, session, rpc_handler}` | `crates/core/src/daemon/...` |
| `src/daemon/server.rs`, `src/main.rs` (Daemon arm) | `crates/broker/src/{main.rs, server.rs}` |
| `src/shim/bridge.rs`, `src/rpc/transport.rs` | `crates/sdk/src/{lib.rs, transport.rs}` |
| `src/{mcp, shim/auto_start.rs}`, `src/main.rs` (shim arm) | `crates/mcp/src/{main.rs, ...}` |

### Three external-facing artifacts

1. The `logmon-broker-sdk` crate (typed Rust API).
2. The `logmon-broker-protocol` crate (transitive Rust dep + canonical types).
3. `protocol-v1.schema.json` (file in protocol crate, suitable for cross-language client codegen).

## Public protocol (v1 frozen)

### Method inventory (unchanged)

The 25 existing RPC methods are frozen as the v1 surface. No method renames, no parameter renames, no result-shape changes for existing methods.

```
logs.recent     logs.context     logs.export     logs.clear
filters.list    filters.add      filters.edit    filters.remove
triggers.list   triggers.add     triggers.edit   triggers.remove
traces.recent   traces.get       traces.summary  traces.slow     traces.logs
spans.context
bookmarks.add   bookmarks.list   bookmarks.remove   bookmarks.clear
session.start   session.list     session.drop
status.get
```

The single notification kind is `notifications/trigger_fired`. Notifications have always been server-pushed via the JSON-RPC notification envelope (no `id` field).

### Three additive extensions

All backward-compatible at the wire level — old request shapes still parse, new fields default to empty/false.

#### 1. `triggers.add` gains `oneshot: bool`

Optional, default `false`. When `true`, the daemon removes the trigger immediately after the first `trigger.fired` notification is enqueued for the owning session.

`triggers.list` results carry the `oneshot` flag.

State persisted into `state.json` for named sessions.

#### 2. `session.start` request gains `client_info`

Optional `client_info: Map<String, JsonValue>`. Free-form JSON object capped at 4 KB serialized. Stored on the session, returned in `session.list` and `status.get`. Strictly informational — the broker never branches on its content.

Recommended shape:

```json
{ "name": "store-test", "version": "1.2.3", "run_id": "run_abc", "host": "..." }
```

If serialized form exceeds 4 KB, daemon rejects `session.start` with error `-32602` (Invalid params) and message `"client_info exceeds 4 KB limit"`.

#### 3. `session.start` response gains `capabilities`

Required in v1. `capabilities: Vec<String>` reports which optional features the daemon supports. v1 always returns at minimum:

```
["bookmarks", "oneshot_triggers", "client_info"]
```

Future capabilities extend the list without bumping `PROTOCOL_VERSION`. Clients negotiate by checking string presence.

### Version policy

- `PROTOCOL_VERSION = 1` is broadcast at handshake. Mismatch = handshake reject (existing behavior).
- **Within v1: additive only.** New optional request fields, new optional notifications, new methods. Existing wire shapes never change within one integer version.
- Breaking changes (rename/remove fields, change semantics, restructure existing results) bump to v2. Old daemons reject new clients; new daemons reject old clients via the existing handshake check.
- `capabilities` provides fine-grained negotiation within a major version.

### JSON Schema export

The `logmon-broker-protocol` crate emits `protocol-v1.schema.json` at build time, walking every `#[derive(JsonSchema)]` struct (request/result/notification). The file is checked into the repo: small enough to diff in PRs, schema drift = obvious code-review signal.

Implementation choice (build.rs vs xtask vs test-emit) is left to the implementation plan. The contract is: the file exists, matches the Rust types, and is regenerated as part of the normal build flow.

External-language clients codegen from the JSON Schema file directly. It is the canonical normative source alongside the Rust types.

CI (or pre-commit hook) verifies schema-file freshness: regenerate, fail if the working tree is dirty after.

### Notification namespace

Spec A reserves `notifications/*` for server-pushed messages. Future kinds (e.g. `notifications/log_matched` for the deferred live-tail subscriptions) are additive within v1 — clients ignore unknown notification methods.

## Binaries: CLI surface and lifecycle

### `logmon-broker`

#### Subcommands

```
logmon-broker                                # run as daemon
logmon-broker install-service [--user|--system]
logmon-broker uninstall-service [--user|--system]
logmon-broker status                         # print "running pid=N socket=PATH" or "not running"
```

Default for `--user|--system` is `--user`.

#### Flags (carry over from today's `Daemon` arm)

```
--gelf-port              UDP+TCP port (default config-driven)
--gelf-udp-port          UDP-only override
--gelf-tcp-port          TCP-only override
--otlp-grpc-port         OTLP gRPC port (0 to disable)
--otlp-http-port         OTLP HTTP port (0 to disable)
--buffer-size            Log ring buffer capacity
--span-buffer-size       Span store capacity
```

Same defaults as today, same `~/.config/logmon/config.json` fallback.

#### Lifecycle

1. **Start.** Bind sockets and receivers; restore named-session triggers/filters from `state.json`; write `daemon.pid`; emit `OTEL:ONLINE` UDP beacon; send systemd `READY=1` via `sd-notify` (no-op when not under systemd); begin accepting connections. The `READY=1` only fires after every receiver is bound, so dependents using `Type=notify` see the broker as ready exactly when it actually is.
2. **Run.** Existing accept loop and RPC dispatch.
3. **Shutdown.** SIGTERM and SIGINT both trigger graceful shutdown: emit `OTEL:OFFLINE`, drain in-flight RPCs, persist `state.json`, remove socket file + pid file, exit 0.
4. **Crash recovery.** PID file may remain. Next start checks for stale PID via `kill -0` and self-cleans; same logic as today's shim auto-start, lifted into the broker's own startup path.

### `logmon-mcp`

```
logmon-mcp [--session NAME]                  # MCP server on stdio
```

No `daemon` subcommand. Same external behavior as today's `logmon-mcp-server`.

#### Broker discovery (replacing `current_exe + "daemon"`)

1. Env override `LOGMON_BROKER_BIN` if set — used as-is.
2. PATH lookup for `logmon-broker`.
3. Sibling fallback: `<dir-of-logmon-mcp>/logmon-broker` (covers `cargo install` with both binaries in the same prefix).
4. If none found: error with a clear message — `"logmon-broker not found; install via 'cargo install --path crates/broker', set LOGMON_BROKER_BIN, or run 'logmon-broker install-service'"`.

The shim's existing `daemon.lock` + pid-check + socket-poll logic is preserved verbatim — only the spawned binary changes.

#### Auto-start coexists with system service

Both paths converge at "is the socket up?" If the broker is already running (started by launchd / systemd, or by an earlier shim), the lock-and-check returns the existing connection. If not, the shim spawns one. No coordination beyond the existing `daemon.lock`.

## System service deployment

### Defaults

User-level (`--user`) by default — no root, no system-wide impact, fits the single-user dev machine. System-level (`--system`) is supported as a flag.

### Service file locations

| Platform | User scope | System scope |
|---|---|---|
| macOS | `~/Library/LaunchAgents/logmon.broker.plist` | `/Library/LaunchDaemons/logmon.broker.plist` (sudo) |
| Linux | `~/.config/systemd/user/logmon-broker.service` | `/etc/systemd/system/logmon-broker.service` (sudo) |

### Template substitution

The plist and unit files are static templates with one substitution: the absolute path of the running `logmon-broker` binary, resolved via `std::env::current_exe()` at install time. This keeps `cargo install` upgrades clean — re-run `logmon-broker install-service` after upgrade and the path picks up the new binary location.

### launchd plist (key fields)

```xml
<key>Label</key>            <string>logmon.broker</string>
<key>ProgramArguments</key> <array><string>{BINARY_PATH}</string></array>
<key>RunAtLoad</key>        <true/>
<key>KeepAlive</key>        <true/>
```

`StandardOutPath` / `StandardErrorPath` omitted: daemon writes to its own `daemon.log` via tracing-appender.

### systemd unit (key fields)

```ini
[Unit]
Description=logmon broker daemon
After=network.target

[Service]
Type=notify
ExecStart={BINARY_PATH}
Restart=on-failure
RestartSec=2s
KillSignal=SIGTERM
TimeoutStopSec=10s

[Install]
WantedBy=default.target
```

`Type=notify` waits for `READY=1` before considering the service started; the broker emits `READY=1` after every receiver is bound.

### Install command behavior

- **install-service**: render template with current `current_exe()`; write file; on macOS run `launchctl bootstrap gui/$(id -u) <plist>` (or `system` for `--system`); on Linux run `systemctl --user daemon-reload && systemctl --user enable --now logmon-broker.service` (or without `--user` for `--system`). Idempotent — re-running overwrites and reloads.
- **uninstall-service**: `bootout` / `disable --now` first; then remove the file; then `daemon-reload`. Safe to run when nothing is installed (silent no-op).
- **status**: pure introspection (defined in §Binaries above); does not touch service state.

### Log routing under a service manager

The daemon's tracing-appender continues to write `~/.config/logmon/daemon.log` regardless of how it was started. Under `--system` install on macOS, `~` resolves to root's home — known wart of `--system`, documented but not fixed in v1. Most users want `--user`.

## SDK shape (`logmon-broker-sdk`)

### Connect-and-handshake

```rust
use logmon_broker_sdk::{Broker, BrokerError, Notification};

let broker = Broker::connect()
    .session_name("store-test-run-abc")           // optional; omit for anonymous
    .client_info(serde_json::json!({               // optional, ≤ 4 KB
        "name":    "store-test",
        "version": env!("CARGO_PKG_VERSION"),
        "run_id":  "abc",
    }))
    .reconnect_max_attempts(10)                    // optional, default 10
    .reconnect_max_backoff(Duration::from_secs(30))
    .open()                                        // performs session.start
    .await?;
```

`Broker::connect()` resolves the socket path: `~/.config/logmon/logmon.sock` on Unix, `127.0.0.1:12200` on Windows. Override with `LOGMON_BROKER_SOCKET` env var (used in tests).

`.open()` returns `Result<Broker, BrokerError>`. The `Broker` handle is `Clone + Send + Sync` (internally `Arc<Inner>`); cloning is cheap and shares the connection.

### Method dispatch

One Rust method per RPC, named after the method-with-underscores: `broker.logs_recent(...)`, `broker.triggers_add(...)`, etc. Each takes a single param struct that derives `Default`:

```rust
let logs = broker.logs_recent(LogsRecent {
    filter: Some(my_filter),
    ..Default::default()
}).await?;

let trigger_id = broker.triggers_add(TriggersAdd {
    filter:  "l>=ERROR".into(),
    oneshot: true,
    description: Some("expect no errors".into()),
    ..Default::default()
}).await?.id;
```

### Typed filter builder

A method per known selector (autocomplete + type system enforce correctness). Stringly-typed access only as an explicit escape hatch for arbitrary GELF additional fields:

```rust
use logmon_broker_sdk::{Filter, Level, SpanStatus, SpanKind};

let f: String = Filter::builder()
    .level_at_least(Level::Error)        // l>=ERROR
    .pattern_regex("/panic|unwrap/", true)
    .message("started")                  // m=started
    .host_regex("/host[0-9]+/", false)
    .span_name("RunScript")
    .span_status(SpanStatus::Error)
    .duration_at_least_ms(100)
    .bookmark_after("test_start")
    .additional_field("_test_id", "run_abc")  // escape hatch
    .build();
```

Methods cover every selector in the frozen grammar (level, host, facility, file, line, message family, span name / service / status / kind, duration, bookmark, pattern, ALL / NONE). Conjunctive only (comma-AND), matching the grammar. Substring + regex variants per selector where applicable. Typed enums for `Level`, `SpanStatus`, `SpanKind`. Approximately 30 methods total — boilerplate but mechanical.

`additional_field(name, value)` and `additional_field_regex(name, pattern, ci)` are the only stringly-typed methods. Consumers wrap them with their own typed helpers (e.g., `StoreTestFilter::test_id(id)` calling `.additional_field("_test_id", id)`).

### Notifications

```rust
let mut sub = broker.subscribe_notifications();
while let Ok(notif) = sub.recv().await {
    match notif {
        Notification::TriggerFired(m) => { /* ... */ }
        Notification::Reconnected { is_new } => { /* SDK-internal — see §Reconnection */ }
        // unknown variants ignored
    }
}
```

`subscribe_notifications()` returns a `tokio::sync::broadcast::Receiver<Notification>`. `Notification` is a `#[non_exhaustive]` enum — clients match on known variants and treat unknowns as no-op. Buffered queue size configurable on the builder (default 100). Lagging consumers see `RecvError::Lagged(n)` per the broadcast contract.

### Errors

Single `BrokerError` enum:

```rust
#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    #[error("transport: {0}")]
    Transport(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("rpc error {code}: {message}")]
    Method { code: i32, message: String },
    #[error("disconnected")]
    Disconnected,
    #[error("session lost (anonymous; cannot resume)")]
    SessionLost,
}
```

### Handle introspection

```rust
broker.session_id() -> &str
broker.is_new_session() -> bool
broker.has_capability(name: &str) -> bool
broker.daemon_uptime() -> Duration
```

### Drop semantics

Dropping the `Broker` closes the connection. Anonymous sessions disappear server-side. Named sessions remain in `state.json`; reconnect via the same `session_name` resumes them.

### Reconnection (named sessions only)

#### Behavior

- Detect disconnect (read returns EOF or transport error) → mark connection state as `Reconnecting`.
- Backoff loop: try to reconnect, exponential with jitter. Defaults: 100 ms initial, 30 s cap, max 10 attempts. All three configurable on the builder.
- On reconnect: re-issue `session.start` with the same session_name + client_info.
- **Named sessions resume cleanly.** Daemon returns `is_new: false`, restores triggers/filters from `state.json`, drains queued notifications. Client's existing trigger/filter IDs remain valid.
- **Anonymous sessions do not resume.** Session is gone server-side; client's local state (trigger IDs, filter IDs, bookmarks) is invalidated. SDK fails loud with `BrokerError::SessionLost`. Caller decides whether to start fresh.
- In-flight RPCs during disconnect: error with `BrokerError::Disconnected`. Caller retries after reconnect.
- Subscribers see `Notification::Reconnected { is_new }` (SDK-internal, not server-pushed) when reconnect completes.

#### Adoption rule for callers

Use named sessions if you want resume semantics. Use anonymous if you accept session-lost-on-restart.

### Out of v1 SDK

- No automatic reconnect-on-disconnect for anonymous sessions (caller's responsibility).
- No built-in `MockBroker` (deferred per discussion; YAGNI).
- No live-tail subscription helper (deferred protocol primitive — flagged as future direction).

## Backward compatibility

Spec A is purely a refactor + minor extension; users see no functional regression.

### Preserved

- Socket path: `~/.config/logmon/logmon.sock` (Unix), `127.0.0.1:12200` (Windows).
- Config dir: `~/.config/logmon/`. `state.json`, `daemon.pid`, `daemon.lock`, `daemon.log` keep exact shapes and locations.
- Protocol: `PROTOCOL_VERSION = 1` with only the three additive extensions. Existing wire shapes parse unchanged.
- MCP tool surface: identical. All 27 tools, all parameters, all behavior.
- OTel beacon: `OTEL:ONLINE` / `OTEL:OFFLINE` to `239.255.77.1:4399`. tracing-init's circuit breaker keeps working without change.

### One non-compat (intentional)

The binary name `logmon-mcp-server` ceases to exist. Replaced by `logmon-broker` + `logmon-mcp`. Existing Claude Code MCP registrations will fail until re-registered. Per single-user-deployment context, no compat shim — clean break.

## Migration

For the existing install:

1. `cd ~/Documents/Projects/MCPs/logmon-mcp && cargo install --path crates/broker --path crates/mcp`
2. `claude mcp remove logmon && claude mcp add logmon -- logmon-mcp`
3. (Optional) `logmon-broker install-service` — puts the daemon under launchd / systemd. Existing auto-start still works as fallback.

Under five minutes. No data migration — `state.json` is read by the new daemon as-is.

## Testing strategy

### Unit tests

Today's `tests/` (21 files) split mechanically:

- Filter / store / pipeline / span tests → `crates/core/tests/`
- Protocol-types serialization tests → `crates/protocol/tests/`
- Auto-start + bridge tests → `crates/mcp/tests/`

### New SDK integration tests

In `crates/sdk/tests/`. Spin up an in-process daemon via `logmon-broker-core`'s programmatic entry point (extracted as a library API), open an SDK client against it, exercise every method and notification kind. The SDK is the system-under-test; the daemon is treated as a black box reached over a real socket.

### New reconnect test

`kill_broker_mid_session`: start broker, open named-session client, kill broker, restart broker, verify SDK transparently resumes the session and triggers / filters survive.

### New service-install smoke (manual checklist, not CI)

```
logmon-broker install-service
launchctl print gui/$(id -u)/logmon.broker        # macOS
systemctl --user status logmon-broker             # Linux
logmon-broker uninstall-service
```

Document in README; not exercised by CI.

### Schema-drift guard

`protocol-v1.schema.json` is committed to the repo and regenerated by the build. CI (or pre-commit hook) fails if a drift is detected without a corresponding schema-file update.

### Integration test scenarios

1. Bare connect → handshake → query → disconnect.
2. Named session create → `triggers.add` (oneshot) → daemon sends matching log → `trigger_fired` delivered → trigger auto-removed → `triggers.list` confirms empty.
3. `client_info` round-trip: `session.start` with payload → `session.list` returns it.
4. Capabilities advertised: every entry from `["bookmarks", "oneshot_triggers", "client_info"]` present.
5. Filter builder produces correct strings for every selector (table-driven test against the existing parser).
6. Reconnect: kill broker, verify backoff retries, restart broker, verify session restored and notifications resume.
7. Anonymous-session reconnect fails with `BrokerError::SessionLost`.
8. Schema-file matches `cargo build` regenerated form.

## Documentation and skill updates

### In `logmon-mcp` repo

- **README.md** — workspace + two-binary architecture; new install commands; system-service section; `LOGMON_BROKER_BIN` and `LOGMON_BROKER_SOCKET` env-var docs.
- **skill/logmon.md** — refreshed architecture summary; broker SDK note (with forward link to Spec B for the store-test integration). Tool list, filter DSL, bookmark docs, multi-session behavior remain verbatim.
- **docs/superpowers/specs/2026-04-30-broker-ification-design.md** — this spec.

### In `Store` repo

- **docs/guides/logmon.md** — binary-name updates if mentioned; brief deployment-options paragraph.
- **CLAUDE.md Resource Catalog** — minor touch on the OpenTelemetry / Tracing row if needed.
- **.claude/CLAUDE.md** — binary-name updates if mentioned.
- **using-store-test skill** — binary-name update if any reference exists; filter-string examples unchanged.

### Spec B will further update

The store-test guides and skills with broker-SDK assertion bindings and the per-test session model. Spec A only covers what is affected by the broker-ification refactor itself.

### Mechanism

Plan-of-record (output of writing-plans, next step) will include explicit doc / skill update tasks alongside code tasks. Skill updates land in the same PR as the change they describe — not as a follow-up — per project's `feedback_skills_as_long_term_memory` rule.

## Future directions

### Live-tail subscriptions (deferred)

A first-class `subscribe.logs(filter)` / `subscribe.spans(filter)` pair returning streamed `notifications/log_matched` / `notifications/span_matched` events. Today this is hacked via `triggers.add` with zero pre / post window — works but the trigger lifecycle and window semantics are not a clean fit. Useful for log-store, dashboards, future IDE plugins.

Designed-for-future, additive within v1. Spec A reserves the `notifications/*` namespace and the `subscribe_logs` / `subscribe_spans` capability strings for this.

### MockBroker SDK helper

A fake broker satisfying the protocol via in-memory state, for client unit tests without a real daemon. Add when first consumer asks (Spec B might).

### Tooling-compat shim

A thin `logmon-mcp-server` wrapper that prints a deprecation warning and `exec`s `logmon-mcp`, for one transition release. Not needed at single-user scale.

## Open questions

None at spec time — all design decisions are committed above.
