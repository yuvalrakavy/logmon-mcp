# Logmon Broker-ification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert `logmon-mcp` from a single-binary, single-crate project into a five-crate Cargo workspace with two binaries (`logmon-broker` + `logmon-mcp`) and a typed Rust SDK, freezing the existing JSON-RPC method surface as v1 with three additive extensions (oneshot triggers, `client_info`, response `capabilities`), shipping launchd / systemd install-service support and `Type=notify` `READY=1` signaling, and adding named-session reconnection in the SDK with cause-propagating error semantics and explicit subscriber-on-disconnect contract.

**Architecture:** Five crates — `logmon-broker-protocol` (typed RPC payloads + schemars-derived JSON Schema; transitively public), `logmon-broker-core` (engine: pipeline / filter / trigger / store / persistence / RPC handler; private), `logmon-broker` (daemon binary), `logmon-broker-sdk` (typed Rust client), `logmon-mcp` (MCP shim binary). JSON-RPC over UDS unchanged. `PROTOCOL_VERSION` stays at 1; extensions are additive at the wire level.

**Tech Stack:** Rust 2021, Cargo workspaces, tokio, serde + serde_json, schemars (JSON Schema codegen), thiserror, rmcp (MCP shim), tonic + opentelemetry-proto (OTLP receiver), socket2 (UDP multicast), sd-notify (systemd READY), tracing + tracing-appender.

**Spec:** [docs/superpowers/specs/2026-04-30-broker-ification-design.md](../specs/2026-04-30-broker-ification-design.md)

---

## File Structure (post-implementation)

```
logmon-mcp/                                 # repo root (renamed nothing)
├── Cargo.toml                              # workspace manifest
├── crates/
│   ├── protocol/
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── lib.rs                      # PROTOCOL_VERSION, RpcRequest/Response/Notification, all method param/result structs
│   │   │   ├── methods.rs                  # typed method param + result structs (LogsRecent, TriggersAdd, ...)
│   │   │   └── notifications.rs            # TriggerFiredPayload + future kinds
│   │   └── protocol-v1.schema.json         # generated, committed
│   ├── core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs                      # programmatic entry: run_in_process()
│   │       ├── engine/                     # was src/engine/
│   │       ├── filter/                     # was src/filter/
│   │       ├── gelf/                       # was src/gelf/
│   │       ├── receiver/                   # was src/receiver/
│   │       ├── span/                       # was src/span/
│   │       ├── store/                      # was src/store/
│   │       └── daemon/                     # was src/daemon/{log_processor, span_processor, persistence, session, rpc_handler}
│   ├── broker/
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── main.rs                     # CLI: default | install-service | uninstall-service | status
│   │   │   ├── server.rs                   # was src/daemon/server.rs (accept loop, lifecycle)
│   │   │   ├── service/
│   │   │   │   ├── mod.rs
│   │   │   │   ├── macos.rs                # launchd plist render + install/uninstall
│   │   │   │   └── linux.rs                # systemd unit render + install/uninstall
│   │   └── templates/
│   │       ├── launchd.plist.template
│   │       └── systemd.service.template
│   ├── sdk/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs                      # public API: Broker, BrokerError, Notification, ...
│   │       ├── connect.rs                  # builder + handshake
│   │       ├── transport.rs                # was src/rpc/transport.rs (newline-delimited JSON I/O)
│   │       ├── bridge.rs                   # was src/shim/bridge.rs (refactored — pending-call dispatch + notification broadcast)
│   │       ├── reconnect.rs                # state machine + backoff
│   │       ├── filter.rs                   # Filter::builder()
│   │       └── methods.rs                  # one Rust fn per RPC, typed-in/typed-out
│   ├── mcp/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs                     # was src/main.rs shim arm
│   │       ├── server.rs                   # was src/mcp/server.rs (rmcp tool registrations)
│   │       ├── notifications.rs            # was src/mcp/notifications.rs (forward SDK notifications to MCP peer)
│   │       └── auto_start.rs               # was src/shim/auto_start.rs (with new broker discovery)
│   └── xtask/
│       ├── Cargo.toml
│       └── src/main.rs                     # cargo xtask gen-schema → writes protocol-v1.schema.json
└── tests/                                  # workspace-level integration tests retained where they straddle crate boundaries
```

---

## Phase 1 — Workspace + crate split (no behavior change)

The goal of this phase is to land a working workspace with all 21 existing tests still passing, no functional change. Each task is a mechanical refactor verified by running the existing test suite.

### Task 1: Set up Cargo workspace skeleton

**Files:**
- Modify: `Cargo.toml` (root) — convert from single-package to workspace manifest
- Create: `crates/protocol/Cargo.toml`
- Create: `crates/core/Cargo.toml`
- Create: `crates/broker/Cargo.toml`
- Create: `crates/sdk/Cargo.toml`
- Create: `crates/mcp/Cargo.toml`
- Create: `crates/xtask/Cargo.toml`
- Create: `crates/{protocol,core,broker,sdk,mcp,xtask}/src/lib.rs` (placeholder — empty stubs)
- Move: existing `src/main.rs` content remains, but in this task we just add the workspace shell around it; src/ continues to compile as the legacy crate temporarily

- [ ] **Step 1: Snapshot existing test count for regression baseline**

```
cargo test 2>&1 | tail -20
```

Record passing count. Should match what we see at end of Phase 1.

- [ ] **Step 2: Convert root Cargo.toml to workspace**

Rewrite `Cargo.toml` to:

```toml
[workspace]
resolver = "2"
members = [
    "crates/protocol",
    "crates/core",
    "crates/broker",
    "crates/sdk",
    "crates/mcp",
    "crates/xtask",
]
default-members = ["crates/broker", "crates/mcp"]

[workspace.package]
version = "0.2.0"
edition = "2021"
authors = ["yuval@villarakavy.com"]
license = "MIT"
repository = "https://github.com/yuval/logmon-mcp"

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = "1"
chrono = { version = "0.4", features = ["serde"] }
regex = "1"
clap = { version = "4", features = ["derive", "env"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tracing-appender = "0.2"
anyhow = "1"
thiserror = "2"
uuid = { version = "1", features = ["v4"] }
fs2 = "0.4"
async-trait = "0.1"
socket2 = "0.5"
tonic = "0.12"
prost = "0.13"
opentelemetry-proto = { version = "0.27", features = ["gen-tonic", "logs", "trace"] }
axum = "0.8"
rmcp = { version = "1.2", features = ["server", "macros", "transport-io"] }
tokio-test = "0.4"
tempfile = "3"
sd-notify = "0.4"
```

Delete the `[package]`, `[dependencies]`, `[dev-dependencies]` sections from root Cargo.toml.

- [ ] **Step 3: Create empty member crate manifests**

Each `crates/*/Cargo.toml` is a minimal placeholder so workspace resolves. Example for `crates/protocol/Cargo.toml`:

```toml
[package]
name = "logmon-broker-protocol"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
serde.workspace = true
serde_json.workspace = true
schemars.workspace = true
```

For `crates/core/Cargo.toml`:

```toml
[package]
name = "logmon-broker-core"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
logmon-broker-protocol = { path = "../protocol" }
tokio.workspace = true
serde.workspace = true
serde_json.workspace = true
chrono.workspace = true
regex.workspace = true
tracing.workspace = true
tracing-appender.workspace = true
anyhow.workspace = true
thiserror.workspace = true
uuid.workspace = true
fs2.workspace = true
async-trait.workspace = true
socket2.workspace = true
tonic.workspace = true
prost.workspace = true
opentelemetry-proto.workspace = true
axum.workspace = true

[dev-dependencies]
tokio-test.workspace = true
tempfile.workspace = true
```

For `crates/broker/Cargo.toml`:

```toml
[package]
name = "logmon-broker"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "logmon-broker"
path = "src/main.rs"

[dependencies]
logmon-broker-protocol = { path = "../protocol" }
logmon-broker-core = { path = "../core" }
tokio.workspace = true
clap.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
anyhow.workspace = true
sd-notify.workspace = true
```

For `crates/sdk/Cargo.toml`:

```toml
[package]
name = "logmon-broker-sdk"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
logmon-broker-protocol = { path = "../protocol" }
tokio.workspace = true
serde.workspace = true
serde_json.workspace = true
chrono.workspace = true
tracing.workspace = true
anyhow.workspace = true
thiserror.workspace = true
fs2.workspace = true

[dev-dependencies]
tokio-test.workspace = true
tempfile.workspace = true
logmon-broker-core = { path = "../core" }   # for spinning up an in-process broker in integration tests
```

For `crates/mcp/Cargo.toml`:

```toml
[package]
name = "logmon-mcp"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "logmon-mcp"
path = "src/main.rs"

[dependencies]
logmon-broker-protocol = { path = "../protocol" }
logmon-broker-sdk = { path = "../sdk" }
rmcp.workspace = true
tokio.workspace = true
clap.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
anyhow.workspace = true
fs2.workspace = true
```

For `crates/xtask/Cargo.toml`:

```toml
[package]
name = "xtask"
version.workspace = true
edition.workspace = true
publish = false

[dependencies]
logmon-broker-protocol = { path = "../protocol" }
schemars.workspace = true
serde_json.workspace = true
clap.workspace = true
anyhow.workspace = true
```

- [ ] **Step 4: Create empty stub `lib.rs` / `main.rs` for each crate**

```rust
// crates/protocol/src/lib.rs
// (will be replaced in Task 2)
```

```rust
// crates/core/src/lib.rs
// (will be replaced in Task 3)
```

```rust
// crates/broker/src/main.rs
fn main() {
    // (will be replaced in Task 4)
}
```

```rust
// crates/sdk/src/lib.rs
// (will be replaced in Task 5)
```

```rust
// crates/mcp/src/main.rs
fn main() {
    // (will be replaced in Task 5)
}
```

```rust
// crates/xtask/src/main.rs
fn main() {
    // (will be replaced in Task 8)
}
```

- [ ] **Step 5: Verify workspace + legacy crate both compile**

```
cargo check --workspace
```

Expected: clean. Legacy `src/` still in tree as the original crate; workspace has stubs alongside. Both compile.

The original src/ will be progressively emptied as Tasks 2–5 migrate code into the new crates.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/
git commit -m "chore(workspace): scaffold five-crate workspace + xtask"
```

---

### Task 2: Move protocol types to `logmon-broker-protocol`

**Files:**
- Move: `src/rpc/types.rs` → `crates/protocol/src/lib.rs` (with module split)
- Create: `crates/protocol/src/methods.rs` (typed method payload structs — empty for now; populated in Task 7)
- Create: `crates/protocol/src/notifications.rs` (typed notification payload structs — empty for now; populated in Task 7)
- Modify: `src/lib.rs` (legacy) — re-export from `logmon-broker-protocol` so existing `crate::rpc::types::*` paths keep resolving during migration
- Modify: legacy `Cargo.toml` (the old src/ crate) — add `logmon-broker-protocol` as a path dep

- [ ] **Step 1: Copy types into new crate**

Copy the content of `src/rpc/types.rs` into `crates/protocol/src/lib.rs`. Keep the existing types unchanged for now (RpcRequest, RpcResponse, RpcError, RpcNotification, DaemonMessage, parse_daemon_message_from_str, SessionStartParams, SessionStartResult, PROTOCOL_VERSION).

Add module declarations at the top:

```rust
pub mod methods;
pub mod notifications;
```

- [ ] **Step 2: Stub method/notification module files**

```rust
// crates/protocol/src/methods.rs
// Typed method param + result structs land here in Task 7.
```

```rust
// crates/protocol/src/notifications.rs
// Typed notification payload structs land here in Task 7.
```

- [ ] **Step 3: Re-export from legacy `src/rpc/types.rs`**

Replace `src/rpc/types.rs` with a re-export shim:

```rust
pub use logmon_broker_protocol::*;
```

Keep `src/rpc/mod.rs` (and `src/rpc/transport.rs`) as-is — `transport.rs` will move in Task 5.

- [ ] **Step 4: Run all existing tests**

```
cargo test --workspace
```

Expected: same passing count as Task 1 Step 1 baseline. No tests broke.

- [ ] **Step 5: Commit**

```bash
git add crates/protocol src/rpc/types.rs Cargo.toml
git commit -m "refactor(protocol): extract logmon-broker-protocol crate"
```

---

### Task 3: Move engine code to `logmon-broker-core`

**Files:**
- Move: `src/engine/`, `src/filter/`, `src/gelf/`, `src/receiver/`, `src/span/`, `src/store/` → `crates/core/src/`
- Move: `src/daemon/log_processor.rs`, `src/daemon/span_processor.rs`, `src/daemon/persistence.rs`, `src/daemon/session.rs`, `src/daemon/rpc_handler.rs` → `crates/core/src/daemon/`
- Modify: `crates/core/src/lib.rs` — declare modules + re-exports
- Modify: legacy `src/lib.rs` — re-export from `logmon-broker-core`
- Modify: legacy crate's `Cargo.toml` — add `logmon-broker-core` as path dep, drop deps that moved

- [ ] **Step 1: Bulk move directories with git**

```bash
git mv src/engine crates/core/src/engine
git mv src/filter crates/core/src/filter
git mv src/gelf crates/core/src/gelf
git mv src/receiver crates/core/src/receiver
git mv src/span crates/core/src/span
git mv src/store crates/core/src/store

mkdir -p crates/core/src/daemon
git mv src/daemon/log_processor.rs crates/core/src/daemon/log_processor.rs
git mv src/daemon/span_processor.rs crates/core/src/daemon/span_processor.rs
git mv src/daemon/persistence.rs crates/core/src/daemon/persistence.rs
git mv src/daemon/session.rs crates/core/src/daemon/session.rs
git mv src/daemon/rpc_handler.rs crates/core/src/daemon/rpc_handler.rs
```

`src/daemon/server.rs` and `src/daemon/mod.rs` stay (they move in Task 4).

- [ ] **Step 2: Author `crates/core/src/lib.rs`**

Replace stub with:

```rust
pub mod engine;
pub mod filter;
pub mod gelf;
pub mod receiver;
pub mod span;
pub mod store;
pub mod daemon {
    pub mod log_processor;
    pub mod persistence;
    pub mod rpc_handler;
    pub mod session;
    pub mod span_processor;
}

// Programmatic entry point used by integration tests in the SDK crate.
// Spins up an in-process daemon listening on a caller-provided socket.
pub use daemon::persistence::DaemonConfig;
```

- [ ] **Step 3: Update import paths inside moved files**

Find any `use crate::rpc::types::...` inside moved files and rewrite to `use logmon_broker_protocol::...`. Find any `use crate::{engine, filter, ...}` that crossed the now-absent boundary and rewrite to `use crate::{...}` (everything is `crate` within the new core crate).

Concretely, run:

```
rg -l "use crate::rpc::types" crates/core/
```

For each hit, replace:

```rust
use crate::rpc::types::{...};
```

with:

```rust
use logmon_broker_protocol::{...};
```

- [ ] **Step 4: Re-export from legacy `src/lib.rs`**

Replace `src/lib.rs` with re-exports so legacy callers keep compiling:

```rust
pub use logmon_broker_core::*;
pub mod rpc {
    pub use logmon_broker_protocol::*;
    pub mod types {
        pub use logmon_broker_protocol::*;
    }
    pub mod transport;  // still in src/rpc/transport.rs until Task 5
}

pub mod config;       // still in src/config.rs until Task 4
pub mod shim;         // still in src/shim/ until Task 5
pub mod mcp;          // still in src/mcp/ until Task 5
pub mod daemon {
    pub use logmon_broker_core::daemon::*;
    pub mod server;   // still in src/daemon/server.rs until Task 4
    pub mod mod_reexport;
}
```

(Adjust to whatever module structure the legacy crate currently exposes — the goal is "no public path becomes broken.")

- [ ] **Step 5: Run all tests**

```
cargo test --workspace
```

Expected: same passing count. Most tests now run against `logmon-broker-core` paths under the hood via the re-exports.

- [ ] **Step 6: Commit**

```bash
git add crates/core src/lib.rs Cargo.toml
git commit -m "refactor(core): extract logmon-broker-core (engine + filter + receiver + stores + daemon plumbing)"
```

---

### Task 4: Move daemon binary to `logmon-broker`

**Files:**
- Move: `src/daemon/server.rs` → `crates/broker/src/server.rs`
- Move: `src/daemon/mod.rs` → delete (its contents are now in `crates/core/src/daemon/`)
- Move: `src/main.rs` Daemon arm → `crates/broker/src/main.rs`
- Move: `src/config.rs` Cli/Commands::Daemon → `crates/broker/src/main.rs` (the daemon-specific CLI)

- [ ] **Step 1: Move server.rs**

```bash
git mv src/daemon/server.rs crates/broker/src/server.rs
```

Update its imports (`use crate::...` → `use logmon_broker_core::...` or `use logmon_broker_protocol::...` as appropriate).

- [ ] **Step 2: Author `crates/broker/src/main.rs`**

Lift the Daemon arm of the old `src/main.rs` into the new binary. Strip the shim-mode branch.

```rust
use clap::Parser;
use logmon_broker_core::daemon::persistence::{config_dir, load_config};
use anyhow::Result;

mod server;

#[derive(Parser, Debug)]
#[command(name = "logmon-broker", version, about = "logmon broker daemon")]
struct Cli {
    #[command(subcommand)]
    command: Option<Subcommand>,

    /// GELF UDP+TCP port (override config)
    #[arg(long)]
    gelf_port: Option<u16>,
    /// GELF UDP-only override
    #[arg(long)]
    gelf_udp_port: Option<u16>,
    /// GELF TCP-only override
    #[arg(long)]
    gelf_tcp_port: Option<u16>,
    /// OTLP gRPC port (0 = disabled)
    #[arg(long)]
    otlp_grpc_port: Option<u16>,
    /// OTLP HTTP port (0 = disabled)
    #[arg(long)]
    otlp_http_port: Option<u16>,
    /// Log ring buffer capacity
    #[arg(long)]
    buffer_size: Option<usize>,
    /// Span store capacity
    #[arg(long)]
    span_buffer_size: Option<usize>,
}

#[derive(Parser, Debug)]
enum Subcommand {
    /// Print broker status (running pid + socket, or "not running")
    Status,
    // install-service / uninstall-service added in Task 23
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Subcommand::Status) => {
            // Implemented in Task 18
            unimplemented!("Task 18")
        }
        None => {
            let mut config = load_config(&config_dir().join("config.json"))?;
            if let Some(p) = cli.gelf_port { config.gelf_port = p; }
            if let Some(p) = cli.gelf_udp_port { config.gelf_udp_port = Some(p); }
            if let Some(p) = cli.gelf_tcp_port { config.gelf_tcp_port = Some(p); }
            if let Some(p) = cli.otlp_grpc_port { config.otlp_grpc_port = p; }
            if let Some(p) = cli.otlp_http_port { config.otlp_http_port = p; }
            if let Some(s) = cli.buffer_size { config.buffer_size = s; }
            if let Some(s) = cli.span_buffer_size { config.span_buffer_size = s; }
            server::run_daemon(config).await
        }
    }
}
```

- [ ] **Step 3: Move `src/config.rs` daemon parts**

`src/config.rs` currently defines `Cli` and `Commands::Daemon`. Replace those with the new shape. The shim Cli moves to `crates/mcp/src/main.rs` in Task 5. Delete `src/config.rs` once both are migrated; until then, keep a stripped-down version that only exposes types still used by legacy `src/main.rs`.

- [ ] **Step 4: Update `src/main.rs` to invoke the new broker (temporary bridge)**

Until Task 5 lands, the old `logmon-mcp-server` binary still exists in legacy `src/`. Make it a thin shim that calls into the new code paths:

```rust
// src/main.rs (temporary)
fn main() -> anyhow::Result<()> {
    // Detect 'daemon' subcommand to preserve old behavior during migration
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("daemon") {
        // Defer to logmon-broker entry — but until that's the install target,
        // call into the daemon code path directly here:
        // ... (mirror the new logic during transition)
    }
    // Otherwise, run the legacy shim
    // ...
    Ok(())
}
```

This keeps the legacy crate working through Task 5. It will be deleted at the end of Task 5.

- [ ] **Step 5: Build the new broker binary**

```
cargo build -p logmon-broker
```

Expected: clean. The binary compiles even without the install-service subcommands (those land in Tasks 18, 23).

- [ ] **Step 6: Run all existing tests**

```
cargo test --workspace
```

Expected: same passing count.

- [ ] **Step 7: Commit**

```bash
git add crates/broker src/main.rs src/daemon
git commit -m "refactor(broker): extract logmon-broker daemon binary"
```

---

### Task 5: Move SDK + shim code to `logmon-broker-sdk` + `logmon-mcp`

**Files:**
- Move: `src/rpc/transport.rs` → `crates/sdk/src/transport.rs`
- Move: `src/shim/bridge.rs` → `crates/sdk/src/bridge.rs`
- Create: `crates/sdk/src/lib.rs` — public API surface
- Create: `crates/sdk/src/connect.rs` — Broker handle + builder
- Move: `src/shim/auto_start.rs` → `crates/mcp/src/auto_start.rs`
- Move: `src/mcp/server.rs` → `crates/mcp/src/server.rs`
- Move: `src/mcp/notifications.rs` → `crates/mcp/src/notifications.rs`
- Move: `src/main.rs` shim arm → `crates/mcp/src/main.rs`
- Delete: legacy `src/`, legacy `Cargo.toml` package section, legacy `tests/` directory (tests follow their owning code in subsequent moves)

- [ ] **Step 1: Move transport + bridge into SDK**

```bash
git mv src/rpc/transport.rs crates/sdk/src/transport.rs
git mv src/shim/bridge.rs crates/sdk/src/bridge.rs
```

Update imports inside both files: `use crate::rpc::types::*` → `use logmon_broker_protocol::*`.

- [ ] **Step 2: Author `crates/sdk/src/lib.rs`**

Minimal public surface for now (typed-method-dispatch and reconnect land in Tasks 13–16):

```rust
pub mod transport;
pub mod bridge;
pub mod connect;

pub use connect::{Broker, BrokerBuilder};
pub use logmon_broker_protocol::{PROTOCOL_VERSION, RpcRequest, RpcResponse, RpcNotification};

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
    #[error("session lost")]
    SessionLost,
}
```

- [ ] **Step 3: Author `crates/sdk/src/connect.rs`**

Initial scaffold — builder + `open()` performing session.start. Reconnect machinery added in Task 16.

```rust
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use serde_json::Value;
use tokio::sync::{broadcast, Mutex};

use crate::bridge::DaemonBridge;
use crate::BrokerError;
use logmon_broker_protocol::{RpcNotification, SessionStartParams, SessionStartResult, PROTOCOL_VERSION};

pub struct BrokerBuilder {
    socket_path: Option<PathBuf>,
    session_name: Option<String>,
    client_info: Option<Value>,
    reconnect_max_attempts: u32,
    reconnect_max_backoff: Duration,
    reconnect_initial_backoff: Duration,
    call_timeout: Duration,
    notification_buffer: usize,
}

impl Default for BrokerBuilder {
    fn default() -> Self {
        Self {
            socket_path: None,
            session_name: None,
            client_info: None,
            reconnect_max_attempts: 10,
            reconnect_max_backoff: Duration::from_secs(30),
            reconnect_initial_backoff: Duration::from_millis(100),
            call_timeout: Duration::from_secs(60),
            notification_buffer: 100,
        }
    }
}

impl BrokerBuilder {
    pub fn session_name(mut self, name: impl Into<String>) -> Self { self.session_name = Some(name.into()); self }
    pub fn client_info(mut self, info: Value) -> Self { self.client_info = Some(info); self }
    pub fn socket_path(mut self, p: PathBuf) -> Self { self.socket_path = Some(p); self }
    pub fn reconnect_max_attempts(mut self, n: u32) -> Self { self.reconnect_max_attempts = n; self }
    pub fn reconnect_max_backoff(mut self, d: Duration) -> Self { self.reconnect_max_backoff = d; self }
    pub fn call_timeout(mut self, d: Duration) -> Self { self.call_timeout = d; self }

    pub async fn open(self) -> Result<Broker, BrokerError> {
        // Resolve socket path — env > builder > default
        let socket_path = self.socket_path
            .or_else(|| std::env::var("LOGMON_BROKER_SOCKET").ok().map(PathBuf::from))
            .unwrap_or_else(default_socket_path);

        // Validate client_info size
        if let Some(ref ci) = self.client_info {
            let serialized = serde_json::to_string(ci).map_err(|e| BrokerError::Protocol(e.to_string()))?;
            if serialized.len() > 4096 {
                return Err(BrokerError::Method {
                    code: -32602,
                    message: "client_info exceeds 4 KB limit".into(),
                });
            }
        }

        // Connect socket
        #[cfg(unix)]
        let stream = tokio::net::UnixStream::connect(&socket_path).await?;
        #[cfg(windows)]
        let stream = tokio::net::TcpStream::connect("127.0.0.1:12200").await?;

        let (notification_tx, _) = broadcast::channel(self.notification_buffer);
        let bridge = DaemonBridge::spawn(stream, notification_tx.clone()).await?;

        // session.start handshake
        let params = SessionStartParams {
            name: self.session_name.clone(),
            protocol_version: PROTOCOL_VERSION,
            // client_info added in Task 11
        };
        let result_value = bridge.call("session.start", serde_json::to_value(&params).unwrap()).await
            .map_err(|e| BrokerError::Protocol(e.to_string()))?;
        let result: SessionStartResult = serde_json::from_value(result_value)
            .map_err(|e| BrokerError::Protocol(e.to_string()))?;

        Ok(Broker {
            inner: Arc::new(Inner {
                bridge: Mutex::new(bridge),
                session_id: result.session_id,
                is_new_session: result.is_new,
                capabilities: vec![], // Task 12
                daemon_uptime: Duration::from_secs(result.daemon_uptime_secs),
                notification_tx,
                config: Arc::new(self),
            })
        })
    }
}

pub struct Broker {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) bridge: Mutex<DaemonBridge>,
    pub(crate) session_id: String,
    pub(crate) is_new_session: bool,
    pub(crate) capabilities: Vec<String>,
    pub(crate) daemon_uptime: Duration,
    pub(crate) notification_tx: broadcast::Sender<RpcNotification>,
    pub(crate) config: Arc<BrokerBuilder>,
}

impl Broker {
    pub fn connect() -> BrokerBuilder { BrokerBuilder::default() }
    pub fn session_id(&self) -> &str { &self.inner.session_id }
    pub fn is_new_session(&self) -> bool { self.inner.is_new_session }
    pub fn has_capability(&self, name: &str) -> bool { self.inner.capabilities.iter().any(|c| c == name) }
    pub fn daemon_uptime(&self) -> Duration { self.inner.daemon_uptime }
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<RpcNotification> {
        self.inner.notification_tx.subscribe()
    }
}

fn default_socket_path() -> PathBuf {
    #[cfg(unix)]
    {
        let dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        dir.join("logmon").join("logmon.sock")
    }
    #[cfg(windows)]
    {
        // TCP path — socket_path unused on Windows; left for symmetry
        PathBuf::from("127.0.0.1:12200")
    }
}
```

Notice: this is the v1 SDK skeleton. `Broker::call_typed`, the typed-method API, the typed Notification enum, the filter builder, and reconnect all land in Phase 4.

Add `dirs = "5"` to `crates/sdk/Cargo.toml`.

- [ ] **Step 4: Move MCP shim files**

```bash
git mv src/shim/auto_start.rs crates/mcp/src/auto_start.rs
git mv src/mcp/server.rs crates/mcp/src/server.rs
git mv src/mcp/notifications.rs crates/mcp/src/notifications.rs
```

Update imports in each: bridge/transport now in `logmon_broker_sdk`; protocol types in `logmon_broker_protocol`.

- [ ] **Step 5: Author `crates/mcp/src/main.rs`**

Lift the shim arm of the old main.rs:

```rust
use clap::Parser;
use logmon_broker_sdk::Broker;
use rmcp::ServiceExt;

mod auto_start;
mod server;
mod notifications;

#[derive(Parser, Debug)]
#[command(name = "logmon-mcp", version, about = "logmon MCP shim")]
struct Cli {
    /// Optional named session
    #[arg(long)]
    session: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("logmon_mcp=info".parse()?),
        )
        .init();

    let cli = Cli::parse();

    // Auto-start broker if needed (Task 21 fills in the discovery logic)
    auto_start::ensure_broker_running().await?;

    let mut builder = Broker::connect();
    if let Some(name) = cli.session {
        builder = builder.session_name(name);
    }
    let broker = builder.open().await?;

    // Start MCP server, forwarding notifications
    let mcp_server = server::GelfMcpServer::new(broker.clone());
    let service = mcp_server.serve(rmcp::transport::stdio()).await?;

    notifications::spawn_notification_forwarder(broker.subscribe_notifications(), service.peer().clone());

    service.waiting().await?;
    Ok(())
}
```

- [ ] **Step 6: Update auto_start.rs broker discovery**

In `crates/mcp/src/auto_start.rs`, replace the `current_exe + "daemon"` logic with the three-tier discovery (this is the discovery-only piece; the install-service flow lives in the broker itself):

```rust
fn locate_broker_binary() -> anyhow::Result<std::path::PathBuf> {
    // 1. Env override
    if let Ok(p) = std::env::var("LOGMON_BROKER_BIN") {
        return Ok(p.into());
    }
    // 2. PATH lookup
    if let Ok(p) = which::which("logmon-broker") {
        return Ok(p);
    }
    // 3. Sibling fallback — look next to ourselves
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let candidate = dir.join("logmon-broker");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!(
        "logmon-broker not found; install via 'cargo install --path crates/broker', \
         set LOGMON_BROKER_BIN, or run 'logmon-broker install-service'"
    );
}
```

Add `which = "6"` to `crates/mcp/Cargo.toml`.

In the spawn path, replace `Command::new(current_exe).arg("daemon")` with `Command::new(locate_broker_binary()?)` (no subcommand argument).

- [ ] **Step 7: Delete legacy src/ and legacy package**

Once everything compiles, remove the legacy crate:

```bash
git rm -r src/
```

Remove any leftover legacy `[package]`/`[dependencies]` sections from root Cargo.toml (they should already be gone from Task 1). The root is now pure `[workspace]`.

- [ ] **Step 8: Build everything**

```
cargo build --workspace
```

Expected: clean. Two binaries built: `target/debug/logmon-broker`, `target/debug/logmon-mcp`.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "refactor(sdk,mcp): extract logmon-broker-sdk + logmon-mcp; delete legacy src/"
```

---

### Task 6: Regression gate — full existing test suite passes

**Files:** No code changes. Verification only.

- [ ] **Step 1: Run full workspace test suite**

```
cargo test --workspace
```

Expected: passing count matches the Phase 1 baseline (Task 1 Step 1). Each pre-existing test that was in `tests/<topic>.rs` should now run under whichever crate it logically belongs to — most under `crates/core/tests/`. If any test ended up in the wrong crate (e.g., a bridge test that's now in mcp/ but only exercises sdk code), move it via `git mv` and re-run.

- [ ] **Step 2: Run logmon-broker binary smoke**

```
target/debug/logmon-broker --help
```

Expected: clap-rendered help text listing flags and subcommands.

```
target/debug/logmon-mcp --help
```

Expected: clap-rendered help with `--session`.

- [ ] **Step 3: Tag Phase 1 completion**

```bash
git tag phase1-complete -m "workspace + crate split, no behavior change, all tests pass"
```

---

## Phase 2 — Protocol typed payloads + JSON Schema export

This phase replaces the existing `Value`-typed `params` and `result` with typed structs in `logmon-broker-protocol`, derives `JsonSchema` on each, and ships the schema-export workflow.

### Task 7: Define typed param + result structs in `logmon-broker-protocol`

**Files:**
- Modify: `crates/protocol/src/methods.rs` — populate with typed structs

- [ ] **Step 1: Add the full typed surface**

Replace `crates/protocol/src/methods.rs` with one struct per RPC param and one per RPC result. Each derives `Serialize`, `Deserialize`, `JsonSchema`, `Default`, `Debug`, `Clone`. Field names match the existing JSON keys exactly.

```rust
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

// ============================================================
// logs.*
// ============================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsRecent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsRecentResult {
    pub logs: Vec<LogEntry>,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsContext {
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
}

// ... LogsContextResult, LogsExport, LogsExportResult, LogsClear, LogsClearResult ...

// ============================================================
// filters.*
// ============================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FiltersList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FiltersListResult {
    pub filters: Vec<FilterInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FilterInfo {
    pub id: u32,
    pub filter: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ... FiltersAdd, FiltersAddResult, FiltersEdit, FiltersEditResult, FiltersRemove, FiltersRemoveResult ...

// ============================================================
// triggers.*
// ============================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersListResult {
    pub triggers: Vec<TriggerInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggerInfo {
    pub id: u32,
    pub filter: String,
    pub pre_window: u32,
    pub post_window: u32,
    pub notify_context: u32,
    #[serde(default)]
    pub oneshot: bool,                                  // added in Task 10
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub match_count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersAdd {
    pub filter: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_context: Option<u32>,
    #[serde(default)]
    pub oneshot: bool,                                  // added in Task 10
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggersAddResult {
    pub id: u32,
}

// ... TriggersEdit, TriggersEditResult, TriggersRemove, TriggersRemoveResult ...

// ============================================================
// traces.* / spans.*
// ============================================================

// ... TracesRecent, TracesRecentResult, TracesGet, TracesGetResult, TracesSummary, TracesSummaryResult,
//     TracesSlow, TracesSlowResult, TracesLogs, TracesLogsResult, SpansContext, SpansContextResult ...

// ============================================================
// bookmarks.*
// ============================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksAdd {
    pub name: String,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BookmarksAddResult {
    pub qualified_name: String,
    pub timestamp: DateTime<Utc>,
    pub replaced: bool,
}

// ... BookmarksList, BookmarksListResult, BookmarksRemove, BookmarksRemoveResult, BookmarksClear, BookmarksClearResult ...

// ============================================================
// session.* / status.*
// ============================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionListResult {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub connected: bool,
    pub trigger_count: usize,
    pub filter_count: usize,
    pub queue_size: usize,
    pub last_seen_secs_ago: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<Value>,                     // added in Task 11
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StatusGet {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StatusGetResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    pub daemon_uptime_secs: u64,
    pub receivers: Vec<String>,
    pub store: StoreStats,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StoreStats {
    pub total_received: u64,
    pub total_stored: u64,
    pub malformed_count: u64,
    pub current_size: usize,
}

// ============================================================
// Shared payload types
// ============================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogEntry {
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    pub level: u8,
    pub host: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub additional_fields: HashMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SpanEntry {
    pub trace_id: String,
    pub span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub name: String,
    pub service: String,
    pub kind: String,
    pub status: SpanStatus,
    pub start_time_unix_nanos: u64,
    pub duration_ms: f64,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub attributes: HashMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SpanStatus {
    Ok,
    #[default]
    Unset,
    Error(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TraceSummary {
    pub trace_id: String,
    pub root_span: String,
    pub start_time_unix_nanos: u64,
    pub duration_ms: f64,
    pub span_count: u32,
    pub log_count: u32,
}
```

(Fill in the elided types analogously — they all follow the same pattern. The full population is verbatim from the existing untyped JSON shapes documented in the spec's protocol surface section.)

Add `schemars` derives + `chrono` workspace dep to `crates/protocol/Cargo.toml`:

```toml
[dependencies]
serde.workspace = true
serde_json.workspace = true
schemars = { workspace = true, features = ["preserve_order", "chrono04"] }
chrono = { workspace = true, features = ["serde"] }
```

- [ ] **Step 2: Update `crates/protocol/src/lib.rs` to re-export**

```rust
pub mod methods;
pub mod notifications;

pub use methods::*;
pub use notifications::*;

pub const PROTOCOL_VERSION: u32 = 1;

// (existing RpcRequest, RpcResponse, RpcError, RpcNotification, DaemonMessage, parse_daemon_message_from_str, SessionStartParams, SessionStartResult)
```

- [ ] **Step 3: Verify protocol crate builds**

```
cargo check -p logmon-broker-protocol
```

Expected: clean.

- [ ] **Step 4: Verify nothing broke (param/result types are additive — daemon still uses Value internally)**

```
cargo test --workspace
```

Expected: all tests still pass. The daemon's RPC handler still operates on `Value` payloads; the typed structs are introduced but not yet consumed (consumed by SDK in Task 13, by daemon optionally in later refactor).

- [ ] **Step 5: Commit**

```bash
git add crates/protocol
git commit -m "feat(protocol): add typed param/result structs for all 25 methods"
```

---

### Task 8: Author xtask gen-schema + emit `protocol-v1.schema.json`

**Files:**
- Modify: `crates/xtask/src/main.rs`
- Create: `crates/protocol/protocol-v1.schema.json` (generated)

- [ ] **Step 1: Author xtask binary**

```rust
// crates/xtask/src/main.rs
use clap::{Parser, Subcommand};
use logmon_broker_protocol::*;
use schemars::schema_for;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate protocol-v1.schema.json
    GenSchema {
        #[arg(long, default_value = "crates/protocol/protocol-v1.schema.json")]
        out: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::GenSchema { out } => gen_schema(&out)?,
    }
    Ok(())
}

fn gen_schema(out: &PathBuf) -> anyhow::Result<()> {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "logmon-broker-protocol-v1",
        "description": format!("logmon broker JSON-RPC protocol, PROTOCOL_VERSION = {}", PROTOCOL_VERSION),
        "version": PROTOCOL_VERSION,
        "definitions": {
            // Method param/result structs
            "LogsRecent":            schema_for!(LogsRecent),
            "LogsRecentResult":      schema_for!(LogsRecentResult),
            "LogsContext":           schema_for!(LogsContext),
            "LogsContextResult":     schema_for!(LogsContextResult),
            "LogsExport":            schema_for!(LogsExport),
            "LogsExportResult":      schema_for!(LogsExportResult),
            "LogsClear":             schema_for!(LogsClear),
            "LogsClearResult":       schema_for!(LogsClearResult),
            "FiltersList":           schema_for!(FiltersList),
            "FiltersListResult":     schema_for!(FiltersListResult),
            "FiltersAdd":            schema_for!(FiltersAdd),
            "FiltersAddResult":      schema_for!(FiltersAddResult),
            "FiltersEdit":           schema_for!(FiltersEdit),
            "FiltersEditResult":     schema_for!(FiltersEditResult),
            "FiltersRemove":         schema_for!(FiltersRemove),
            "FiltersRemoveResult":   schema_for!(FiltersRemoveResult),
            "TriggersList":          schema_for!(TriggersList),
            "TriggersListResult":    schema_for!(TriggersListResult),
            "TriggersAdd":           schema_for!(TriggersAdd),
            "TriggersAddResult":     schema_for!(TriggersAddResult),
            "TriggersEdit":          schema_for!(TriggersEdit),
            "TriggersEditResult":    schema_for!(TriggersEditResult),
            "TriggersRemove":        schema_for!(TriggersRemove),
            "TriggersRemoveResult":  schema_for!(TriggersRemoveResult),
            "TracesRecent":          schema_for!(TracesRecent),
            "TracesRecentResult":    schema_for!(TracesRecentResult),
            "TracesGet":             schema_for!(TracesGet),
            "TracesGetResult":       schema_for!(TracesGetResult),
            "TracesSummary":         schema_for!(TracesSummary),
            "TracesSummaryResult":   schema_for!(TracesSummaryResult),
            "TracesSlow":            schema_for!(TracesSlow),
            "TracesSlowResult":      schema_for!(TracesSlowResult),
            "TracesLogs":            schema_for!(TracesLogs),
            "TracesLogsResult":      schema_for!(TracesLogsResult),
            "SpansContext":          schema_for!(SpansContext),
            "SpansContextResult":    schema_for!(SpansContextResult),
            "BookmarksAdd":          schema_for!(BookmarksAdd),
            "BookmarksAddResult":    schema_for!(BookmarksAddResult),
            "BookmarksList":         schema_for!(BookmarksList),
            "BookmarksListResult":   schema_for!(BookmarksListResult),
            "BookmarksRemove":       schema_for!(BookmarksRemove),
            "BookmarksRemoveResult": schema_for!(BookmarksRemoveResult),
            "BookmarksClear":        schema_for!(BookmarksClear),
            "BookmarksClearResult":  schema_for!(BookmarksClearResult),
            "SessionList":           schema_for!(SessionList),
            "SessionListResult":     schema_for!(SessionListResult),
            "SessionDrop":           schema_for!(SessionDrop),
            "SessionDropResult":     schema_for!(SessionDropResult),
            "StatusGet":             schema_for!(StatusGet),
            "StatusGetResult":       schema_for!(StatusGetResult),
            "SessionStartParams":    schema_for!(SessionStartParams),
            "SessionStartResult":    schema_for!(SessionStartResult),
            // Notification payloads
            "TriggerFiredPayload":   schema_for!(TriggerFiredPayload),
            // Shared types
            "LogEntry":              schema_for!(LogEntry),
            "SpanEntry":             schema_for!(SpanEntry),
            "SpanStatus":            schema_for!(SpanStatus),
            "TraceSummary":          schema_for!(TraceSummary),
            "FilterInfo":            schema_for!(FilterInfo),
            "TriggerInfo":           schema_for!(TriggerInfo),
            "SessionInfo":           schema_for!(SessionInfo),
            "StoreStats":            schema_for!(StoreStats),
        }
    });
    let pretty = serde_json::to_string_pretty(&schema)?;
    fs::write(out, format!("{pretty}\n"))?;
    println!("wrote {}", out.display());
    Ok(())
}
```

(Note: `TriggerFiredPayload` is added in Task 7 Step 1's `notifications.rs` — populate it now if not yet done. See Task 14 for the typed enum on the SDK side.)

- [ ] **Step 2: Add `[alias]` for `cargo xtask`**

Create `.cargo/config.toml` at repo root:

```toml
[alias]
xtask = "run --quiet --package xtask --"
```

- [ ] **Step 3: Run xtask gen-schema**

```
cargo xtask gen-schema
```

Expected: writes `crates/protocol/protocol-v1.schema.json`. Verify file size > 10 KB, contains all definitions.

- [ ] **Step 4: Commit**

```bash
git add .cargo/config.toml crates/xtask crates/protocol/protocol-v1.schema.json
git commit -m "feat(xtask): cargo xtask gen-schema → protocol-v1.schema.json"
```

---

### Task 9: Schema-drift guard

**Files:**
- Create: `crates/xtask/src/main.rs` — extend with `verify-schema` subcommand
- Create: `.github/workflows/ci.yml` if missing, or add a step to existing CI (if no CI exists, ship a pre-commit hook script in `scripts/check-schema-drift.sh`)

- [ ] **Step 1: Add verify-schema subcommand**

In `crates/xtask/src/main.rs`, add:

```rust
#[derive(Subcommand)]
enum Cmd {
    GenSchema { #[arg(long, default_value = "crates/protocol/protocol-v1.schema.json")] out: PathBuf },
    /// Regenerate schema and fail if it differs from the committed file
    VerifySchema { #[arg(long, default_value = "crates/protocol/protocol-v1.schema.json")] expected: PathBuf },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::GenSchema { out } => gen_schema(&out)?,
        Cmd::VerifySchema { expected } => verify_schema(&expected)?,
    }
    Ok(())
}

fn verify_schema(expected: &PathBuf) -> anyhow::Result<()> {
    let tmp = tempfile::NamedTempFile::new()?;
    gen_schema(&tmp.path().to_path_buf())?;
    let on_disk = fs::read_to_string(expected)?;
    let regenerated = fs::read_to_string(tmp.path())?;
    if on_disk != regenerated {
        anyhow::bail!(
            "{} is out of date — run 'cargo xtask gen-schema' and commit the result",
            expected.display()
        );
    }
    println!("schema {} is up to date", expected.display());
    Ok(())
}
```

Add `tempfile = "3"` to xtask dependencies.

- [ ] **Step 2: Add a pre-commit-style script**

Create `scripts/check-schema-drift.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail
cargo xtask verify-schema
```

`chmod +x scripts/check-schema-drift.sh`.

- [ ] **Step 3: Verify**

```
cargo xtask verify-schema
```

Expected: `schema crates/protocol/protocol-v1.schema.json is up to date`.

Now corrupt and re-verify:

```
echo "{}" > crates/protocol/protocol-v1.schema.json
cargo xtask verify-schema
```

Expected: error with "out of date — run 'cargo xtask gen-schema'".

Restore:

```
cargo xtask gen-schema
```

- [ ] **Step 4: Commit**

```bash
git add crates/xtask/src/main.rs crates/xtask/Cargo.toml scripts/check-schema-drift.sh
git commit -m "feat(xtask): verify-schema subcommand for schema-drift guard"
```

---

## Phase 3 — Three additive extensions

### Task 10: Add `oneshot` to `triggers.add`

**Files:**
- Modify: `crates/core/src/engine/trigger.rs` — add `oneshot: bool` to `TriggerInfo`, `TriggerSpec`
- Modify: `crates/core/src/daemon/session.rs` — pass through oneshot in add/edit, persist
- Modify: `crates/core/src/daemon/persistence.rs` — extend `PersistedTrigger` shape
- Modify: `crates/core/src/daemon/rpc_handler.rs` — read oneshot param, return in list result
- Modify: `crates/core/src/engine/pipeline.rs` (or wherever trigger_fired is dispatched) — remove trigger when oneshot fires
- Modify: `crates/protocol/src/methods.rs` — `oneshot` field already added in Task 7
- Modify: `crates/protocol/src/notifications.rs` — TriggerFiredPayload has `oneshot` field
- Test: `crates/core/tests/oneshot_triggers.rs` (new)

- [ ] **Step 1: Write the failing test**

Create `crates/core/tests/oneshot_triggers.rs`:

```rust
use logmon_broker_core::{spawn_test_daemon, TestDaemonHandle};
use logmon_broker_protocol::{TriggersAdd, TriggersAddResult, TriggersList, TriggersListResult};
use serde_json::json;

#[tokio::test]
async fn oneshot_trigger_removes_after_first_match() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // Add a oneshot trigger matching ERROR-level logs
    let resp: TriggersAddResult = client.call("triggers.add", json!({
        "filter": "l>=ERROR",
        "oneshot": true,
        "description": "oneshot test"
    })).await.unwrap();
    let trigger_id = resp.id;

    // Inject one ERROR log → trigger should fire
    daemon.inject_log("error", "first error").await;
    let _notification = client.expect_trigger_fired(trigger_id).await;

    // Verify trigger removed via triggers.list
    let list: TriggersListResult = client.call("triggers.list", json!({})).await.unwrap();
    assert!(!list.triggers.iter().any(|t| t.id == trigger_id),
        "oneshot trigger {} should be removed; got {:?}", trigger_id, list.triggers);

    // Inject another ERROR — should NOT fire trigger
    daemon.inject_log("error", "second error").await;
    let no_notification = client.try_recv_notification(std::time::Duration::from_millis(200)).await;
    assert!(no_notification.is_none(), "expected no notification after oneshot removal");
}

#[tokio::test]
async fn non_oneshot_trigger_persists() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let resp: TriggersAddResult = client.call("triggers.add", json!({
        "filter": "l>=ERROR",
        // oneshot defaults to false
    })).await.unwrap();
    let trigger_id = resp.id;

    daemon.inject_log("error", "first error").await;
    let _ = client.expect_trigger_fired(trigger_id).await;

    let list: TriggersListResult = client.call("triggers.list", json!({})).await.unwrap();
    assert!(list.triggers.iter().any(|t| t.id == trigger_id && !t.oneshot));

    daemon.inject_log("error", "second error").await;
    let _ = client.expect_trigger_fired(trigger_id).await;  // fires again
}
```

The `spawn_test_daemon` helper, `connect_anon`, `inject_log`, `expect_trigger_fired`, `try_recv_notification` belong in a new test-support module — `crates/core/src/test_support.rs` (gated `#[cfg(any(test, feature = "test-support"))]`). Skeleton in Step 2.

- [ ] **Step 2: Add test_support module**

Create `crates/core/src/test_support.rs`:

```rust
//! In-process daemon harness for integration tests.

use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::io::{BufReader, AsyncBufReadExt, AsyncWriteExt};
use serde_json::{json, Value};
use logmon_broker_protocol::{RpcRequest, RpcResponse, RpcNotification, DaemonMessage, parse_daemon_message_from_str};

pub struct TestDaemonHandle {
    pub socket_path: PathBuf,
    pub _tempdir: TempDir,
    pub log_inject: tokio::sync::mpsc::Sender<crate::gelf::message::LogEntry>,
}

impl TestDaemonHandle {
    pub async fn inject_log(&self, level: &str, message: &str) { /* push synthetic LogEntry */ }
    pub async fn connect_anon(&self) -> TestClient { /* ... */ }
}

pub struct TestClient {
    /* writer, reader task, pending map, etc. */
}

impl TestClient {
    pub async fn call<R: serde::de::DeserializeOwned>(&mut self, method: &str, params: Value) -> anyhow::Result<R> { /* ... */ }
    pub async fn expect_trigger_fired(&mut self, trigger_id: u32) -> RpcNotification { /* ... */ }
    pub async fn try_recv_notification(&mut self, timeout: std::time::Duration) -> Option<RpcNotification> { /* ... */ }
}

pub async fn spawn_test_daemon() -> TestDaemonHandle {
    let tempdir = TempDir::new().unwrap();
    let socket_path = tempdir.path().join("test.sock");
    // Spawn an in-process daemon on socket_path with synthetic GELF/log injection
    // (Use the same plumbing as crates/broker but with overrides for paths/ports)
    todo!("flesh out — invoke crates/broker's run_daemon with overridden config")
}
```

Implement the harness sufficient for the tests above. The daemon's accept loop and RPC handler are in `logmon-broker-core` already (after Task 4 moved server.rs to broker, the accept loop is in broker — re-export the inner `handle_connection` function from broker as a library function or move it back to core). Pragmatic choice: expose a `pub async fn run_in_process(config, socket_path) -> ...` from `logmon-broker-core` that broker also uses.

Adjust `crates/core/src/lib.rs` to export this entry point:

```rust
pub fn run_in_process_args() -> /* … */ { /* … */ }

#[cfg(feature = "test-support")]
pub mod test_support;
```

Add `[features]` section to `crates/core/Cargo.toml`:

```toml
[features]
test-support = ["dep:tempfile"]
```

And to dev-dependencies in core:
```toml
tempfile.workspace = true
```

- [ ] **Step 3: Run failing test**

```
cargo test -p logmon-broker-core --features test-support --test oneshot_triggers
```

Expected: FAIL — `oneshot` field handling not implemented in pipeline.

- [ ] **Step 4: Implement oneshot in trigger machinery**

Modify `crates/core/src/engine/trigger.rs` — add `oneshot: bool` to `TriggerSpec` and `TriggerInfo`:

```rust
pub struct TriggerInfo {
    pub id: u32,
    pub filter_string: String,
    pub condition: ParsedFilter,
    pub pre_window: u32,
    pub post_window: u32,
    pub notify_context: u32,
    pub description: Option<String>,
    pub oneshot: bool,           // NEW
    pub match_count: u64,
}
```

Modify `add_trigger` signature to accept `oneshot: bool`, default false.

In `crates/core/src/engine/pipeline.rs` (or wherever trigger fires), after dispatching the notification:

```rust
// In trigger evaluation path — when a trigger matches and notification is enqueued:
if trigger.oneshot {
    self.sessions.remove_trigger(&session_id, trigger.id).ok();
}
```

Modify `crates/core/src/daemon/rpc_handler.rs::handle_triggers_add` to read `oneshot` from params:

```rust
let oneshot = params.get("oneshot").and_then(|v| v.as_bool()).unwrap_or(false);
let id = self.sessions.add_trigger(session_id, filter, pre, post, ctx, desc, oneshot)
    .map_err(|e| e.to_string())?;
```

Modify `handle_triggers_list` to include `oneshot` in result.

Modify `crates/core/src/daemon/persistence.rs::PersistedTrigger` to add `#[serde(default)] oneshot: bool` (existing state.json files load with `false` default).

- [ ] **Step 5: Run test, expect pass**

```
cargo test -p logmon-broker-core --features test-support --test oneshot_triggers
```

Expected: PASS — both `oneshot_trigger_removes_after_first_match` and `non_oneshot_trigger_persists`.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(triggers): add oneshot flag — trigger auto-removes after first fire"
```

---

### Task 11: Add `client_info` to `session.start`

**Files:**
- Modify: `crates/protocol/src/lib.rs` — add `client_info` field to `SessionStartParams`
- Modify: `crates/core/src/daemon/session.rs` — store client_info on session
- Modify: `crates/core/src/daemon/server.rs` (or wherever session.start handler lives) — validate 4 KB cap, store
- Modify: `crates/core/src/daemon/rpc_handler.rs::handle_session_list` / `handle_status` — surface client_info
- Modify: `crates/core/src/daemon/persistence.rs` — persist client_info for named sessions
- Test: `crates/core/tests/client_info.rs` (new)

- [ ] **Step 1: Write the failing test**

```rust
// crates/core/tests/client_info.rs
use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{SessionListResult};
use serde_json::json;

#[tokio::test]
async fn client_info_round_trip() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_named("test_session", Some(json!({
        "name": "store-test",
        "version": "0.1.0",
        "run_id": "abc"
    }))).await;

    let list: SessionListResult = client.call("session.list", json!({})).await.unwrap();
    let session = list.sessions.iter().find(|s| s.name.as_deref() == Some("test_session")).unwrap();
    let ci = session.client_info.as_ref().expect("client_info present");
    assert_eq!(ci["name"], "store-test");
    assert_eq!(ci["version"], "0.1.0");
    assert_eq!(ci["run_id"], "abc");
}

#[tokio::test]
async fn client_info_size_limit() {
    let daemon = spawn_test_daemon().await;
    // Build a payload > 4 KB
    let huge = "x".repeat(5000);
    let result = daemon.try_connect_named("oversize", Some(json!({"name": huge}))).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("client_info exceeds 4 KB limit"), "got: {err}");
}
```

- [ ] **Step 2: Update SessionStartParams**

In `crates/protocol/src/lib.rs`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionStartParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub protocol_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<Value>,
}
```

- [ ] **Step 3: Server-side handling**

In the session.start handler (likely in `crates/core/src/daemon/server.rs`), after parsing params:

```rust
if let Some(ci) = &params.client_info {
    let serialized = serde_json::to_string(ci).unwrap_or_default();
    if serialized.len() > 4096 {
        let resp = RpcResponse::error(
            request.id,
            -32602,
            "client_info exceeds 4 KB limit",
        );
        write_message(&mut writer, &resp).await?;
        return Ok(());
    }
}
// Store on session
sessions.set_client_info(&session_id, params.client_info.clone());
```

In `crates/core/src/daemon/session.rs`:

```rust
pub struct SessionState {
    // ... existing fields
    pub client_info: RwLock<Option<Value>>,
}
```

`set_client_info` writes through to the RwLock. `SessionInfo` (the public projection) includes `client_info`.

- [ ] **Step 4: Persist for named sessions**

In `crates/core/src/daemon/persistence.rs::PersistedSession`:

```rust
pub struct PersistedSession {
    pub triggers: Vec<PersistedTrigger>,
    pub filters: Vec<PersistedFilter>,
    #[serde(default)]
    pub client_info: Option<Value>,
}
```

Restore on load; save on save.

- [ ] **Step 5: Surface in session.list and status.get**

Modify `handle_session_list` in `rpc_handler.rs`:

```rust
let items: Vec<Value> = sessions.iter().map(|s| {
    json!({
        "id": s.id.to_string(),
        "name": s.name,
        "connected": s.connected,
        // ...
        "client_info": s.client_info,
    })
}).collect();
```

Same for `handle_status`.

- [ ] **Step 6: Run test, expect pass**

```
cargo test -p logmon-broker-core --features test-support --test client_info
```

Expected: PASS — both `client_info_round_trip` and `client_info_size_limit`.

- [ ] **Step 7: Regenerate schema + commit**

```bash
cargo xtask gen-schema
git add -A
git commit -m "feat(session): add client_info to session.start with 4 KB cap; persist for named sessions"
```

---

### Task 12: Add `capabilities` to `session.start` response

**Files:**
- Modify: `crates/protocol/src/lib.rs` — add `capabilities: Vec<String>` to `SessionStartResult`
- Modify: `crates/core/src/daemon/rpc_handler.rs::build_session_start_result` — populate capabilities
- Test: `crates/core/tests/capabilities.rs` (new)

- [ ] **Step 1: Write the failing test**

```rust
// crates/core/tests/capabilities.rs
use logmon_broker_core::test_support::*;
use logmon_broker_protocol::SessionStartResult;
use serde_json::json;

#[tokio::test]
async fn v1_capabilities_advertised() {
    let daemon = spawn_test_daemon().await;
    let result: SessionStartResult = daemon.session_start(None, None).await.unwrap();
    assert!(result.capabilities.contains(&"bookmarks".to_string()));
    assert!(result.capabilities.contains(&"oneshot_triggers".to_string()));
    assert!(result.capabilities.contains(&"client_info".to_string()));
}
```

- [ ] **Step 2: Update SessionStartResult**

In `crates/protocol/src/lib.rs`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionStartResult {
    pub session_id: String,
    pub is_new: bool,
    pub queued_notifications: usize,
    pub trigger_count: usize,
    pub filter_count: usize,
    pub daemon_uptime_secs: u64,
    pub buffer_size: usize,
    pub receivers: Vec<String>,
    pub capabilities: Vec<String>,
}
```

- [ ] **Step 3: Populate in build_session_start_result**

In `crates/core/src/daemon/rpc_handler.rs`:

```rust
pub fn build_session_start_result(&self, session_id: &SessionId) -> SessionStartResult {
    let info = self.sessions.get(session_id);
    SessionStartResult {
        session_id: session_id.to_string(),
        is_new: true,
        queued_notifications: info.as_ref().map_or(0, |i| i.queue_size),
        trigger_count: info.as_ref().map_or(0, |i| i.trigger_count),
        filter_count: info.as_ref().map_or(0, |i| i.filter_count),
        daemon_uptime_secs: self.start_time.elapsed().as_secs(),
        buffer_size: self.pipeline.store_len(),
        receivers: self.receivers_info.clone(),
        capabilities: vec![
            "bookmarks".into(),
            "oneshot_triggers".into(),
            "client_info".into(),
        ],
    }
}
```

- [ ] **Step 4: Run test, expect pass**

```
cargo test -p logmon-broker-core --features test-support --test capabilities
```

Expected: PASS.

- [ ] **Step 5: Regenerate schema + commit**

```bash
cargo xtask gen-schema
git add -A
git commit -m "feat(session): advertise capabilities in session.start response (v1: bookmarks, oneshot_triggers, client_info)"
```

---

## Phase 4 — SDK typed surface

### Task 13: Typed method dispatch in SDK

**Files:**
- Create: `crates/sdk/src/methods.rs` — one Rust fn per RPC method
- Modify: `crates/sdk/src/lib.rs` — re-export
- Modify: `crates/sdk/src/connect.rs` — `Broker::call_typed` helper
- Test: `crates/sdk/tests/methods.rs` (new)

- [ ] **Step 1: Write the failing test**

```rust
// crates/sdk/tests/methods.rs
use logmon_broker_sdk::Broker;
use logmon_broker_protocol::{LogsRecent, TriggersAdd};
use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;

#[tokio::test]
async fn typed_methods_round_trip() {
    let daemon = spawn_test_daemon_for_sdk().await;
    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .open()
        .await
        .unwrap();

    // logs.recent — typed param, typed result
    let result = broker.logs_recent(LogsRecent {
        count: Some(10),
        ..Default::default()
    }).await.unwrap();
    assert!(result.logs.is_empty());
    assert_eq!(result.count, 0);

    // triggers.add with oneshot
    let added = broker.triggers_add(TriggersAdd {
        filter: "l>=ERROR".into(),
        oneshot: true,
        description: Some("test".into()),
        ..Default::default()
    }).await.unwrap();
    assert!(added.id > 0);
}
```

- [ ] **Step 2: Run test, expect failure**

```
cargo test -p logmon-broker-sdk --test methods
```

Expected: FAIL — `broker.logs_recent` not defined.

- [ ] **Step 3: Implement typed dispatch**

Add to `crates/sdk/src/connect.rs` on the `Broker` impl:

```rust
impl Broker {
    pub async fn call_typed<P, R>(&self, method: &str, params: P) -> Result<R, BrokerError>
    where
        P: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let value = serde_json::to_value(&params)
            .map_err(|e| BrokerError::Protocol(e.to_string()))?;
        let result_value = self.inner.bridge.lock().await
            .call(method, value)
            .await
            .map_err(map_bridge_error)?;
        serde_json::from_value(result_value)
            .map_err(|e| BrokerError::Protocol(e.to_string()))
    }
}

fn map_bridge_error(e: anyhow::Error) -> BrokerError {
    // Convert string-based bridge error to typed BrokerError variants
    let msg = e.to_string();
    if let Some((code, message)) = parse_rpc_error(&msg) {
        BrokerError::Method { code, message }
    } else {
        BrokerError::Protocol(msg)
    }
}
```

Author `crates/sdk/src/methods.rs` — one method per RPC:

```rust
use crate::{Broker, BrokerError};
use logmon_broker_protocol::*;

impl Broker {
    pub async fn logs_recent(&self, p: LogsRecent) -> Result<LogsRecentResult, BrokerError> {
        self.call_typed("logs.recent", p).await
    }
    pub async fn logs_context(&self, p: LogsContext) -> Result<LogsContextResult, BrokerError> {
        self.call_typed("logs.context", p).await
    }
    pub async fn logs_export(&self, p: LogsExport) -> Result<LogsExportResult, BrokerError> {
        self.call_typed("logs.export", p).await
    }
    pub async fn logs_clear(&self, p: LogsClear) -> Result<LogsClearResult, BrokerError> {
        self.call_typed("logs.clear", p).await
    }
    pub async fn filters_list(&self, p: FiltersList) -> Result<FiltersListResult, BrokerError> {
        self.call_typed("filters.list", p).await
    }
    pub async fn filters_add(&self, p: FiltersAdd) -> Result<FiltersAddResult, BrokerError> {
        self.call_typed("filters.add", p).await
    }
    pub async fn filters_edit(&self, p: FiltersEdit) -> Result<FiltersEditResult, BrokerError> {
        self.call_typed("filters.edit", p).await
    }
    pub async fn filters_remove(&self, p: FiltersRemove) -> Result<FiltersRemoveResult, BrokerError> {
        self.call_typed("filters.remove", p).await
    }
    pub async fn triggers_list(&self, p: TriggersList) -> Result<TriggersListResult, BrokerError> {
        self.call_typed("triggers.list", p).await
    }
    pub async fn triggers_add(&self, p: TriggersAdd) -> Result<TriggersAddResult, BrokerError> {
        self.call_typed("triggers.add", p).await
    }
    pub async fn triggers_edit(&self, p: TriggersEdit) -> Result<TriggersEditResult, BrokerError> {
        self.call_typed("triggers.edit", p).await
    }
    pub async fn triggers_remove(&self, p: TriggersRemove) -> Result<TriggersRemoveResult, BrokerError> {
        self.call_typed("triggers.remove", p).await
    }
    pub async fn traces_recent(&self, p: TracesRecent) -> Result<TracesRecentResult, BrokerError> {
        self.call_typed("traces.recent", p).await
    }
    pub async fn traces_get(&self, p: TracesGet) -> Result<TracesGetResult, BrokerError> {
        self.call_typed("traces.get", p).await
    }
    pub async fn traces_summary(&self, p: TracesSummary) -> Result<TracesSummaryResult, BrokerError> {
        self.call_typed("traces.summary", p).await
    }
    pub async fn traces_slow(&self, p: TracesSlow) -> Result<TracesSlowResult, BrokerError> {
        self.call_typed("traces.slow", p).await
    }
    pub async fn traces_logs(&self, p: TracesLogs) -> Result<TracesLogsResult, BrokerError> {
        self.call_typed("traces.logs", p).await
    }
    pub async fn spans_context(&self, p: SpansContext) -> Result<SpansContextResult, BrokerError> {
        self.call_typed("spans.context", p).await
    }
    pub async fn bookmarks_add(&self, p: BookmarksAdd) -> Result<BookmarksAddResult, BrokerError> {
        self.call_typed("bookmarks.add", p).await
    }
    pub async fn bookmarks_list(&self, p: BookmarksList) -> Result<BookmarksListResult, BrokerError> {
        self.call_typed("bookmarks.list", p).await
    }
    pub async fn bookmarks_remove(&self, p: BookmarksRemove) -> Result<BookmarksRemoveResult, BrokerError> {
        self.call_typed("bookmarks.remove", p).await
    }
    pub async fn bookmarks_clear(&self, p: BookmarksClear) -> Result<BookmarksClearResult, BrokerError> {
        self.call_typed("bookmarks.clear", p).await
    }
    pub async fn session_list(&self, p: SessionList) -> Result<SessionListResult, BrokerError> {
        self.call_typed("session.list", p).await
    }
    pub async fn session_drop(&self, p: SessionDrop) -> Result<SessionDropResult, BrokerError> {
        self.call_typed("session.drop", p).await
    }
    pub async fn status_get(&self, p: StatusGet) -> Result<StatusGetResult, BrokerError> {
        self.call_typed("status.get", p).await
    }
}
```

Re-export from `crates/sdk/src/lib.rs`:

```rust
mod methods;
```

- [ ] **Step 4: Run test, expect pass**

```
cargo test -p logmon-broker-sdk --test methods
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(sdk): typed method dispatch — one Rust fn per RPC, typed param + typed result"
```

---

### Task 14: Notification typed enum + `Reconnected` variant

**Files:**
- Modify: `crates/protocol/src/notifications.rs` — `TriggerFiredPayload`
- Modify: `crates/sdk/src/lib.rs` — `Notification` enum
- Modify: `crates/sdk/src/bridge.rs` — convert `RpcNotification` → `Notification` enum on send
- Modify: `crates/sdk/src/connect.rs` — `subscribe_notifications` returns `broadcast::Receiver<Notification>` instead of raw `RpcNotification`
- Test: `crates/sdk/tests/notifications.rs` (new)

- [ ] **Step 1: Add typed payload struct in protocol**

In `crates/protocol/src/notifications.rs`:

```rust
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use crate::LogEntry;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggerFiredPayload {
    pub trigger_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub filter_string: String,
    pub pre_window: u32,
    pub post_window: u32,
    pub notify_context: u32,
    #[serde(default)]
    pub oneshot: bool,
    pub matched_entry: LogEntry,
    #[serde(default)]
    pub context_before: Vec<LogEntry>,
}
```

- [ ] **Step 2: Define Notification enum in SDK**

In `crates/sdk/src/lib.rs`:

```rust
use logmon_broker_protocol::TriggerFiredPayload;

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Notification {
    TriggerFired(TriggerFiredPayload),
    Reconnected,
}
```

- [ ] **Step 3: Convert RpcNotification → Notification in bridge**

Modify `crates/sdk/src/bridge.rs`:

The bridge previously used `broadcast::Sender<RpcNotification>`. Replace with `broadcast::Sender<Notification>`. In the reader loop, when an `RpcNotification` arrives:

```rust
match notif.method.as_str() {
    "notifications/trigger_fired" | "trigger.fired" => {
        match serde_json::from_value::<TriggerFiredPayload>(notif.params.clone()) {
            Ok(payload) => {
                let _ = notification_tx.send(Notification::TriggerFired(payload));
            }
            Err(e) => {
                tracing::warn!(error=%e, "failed to parse trigger_fired payload");
            }
        }
    }
    other => {
        tracing::debug!(method=%other, "ignoring unknown notification");
    }
}
```

- [ ] **Step 4: Update Broker.subscribe_notifications signature**

```rust
pub fn subscribe_notifications(&self) -> broadcast::Receiver<Notification> {
    self.inner.notification_tx.subscribe()
}
```

The internal `notification_tx` is now `broadcast::Sender<Notification>`.

- [ ] **Step 5: Test**

```rust
// crates/sdk/tests/notifications.rs
use logmon_broker_sdk::{Broker, Notification};
use logmon_broker_protocol::TriggersAdd;
use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;

#[tokio::test]
async fn typed_trigger_fired_received() {
    let daemon = spawn_test_daemon_for_sdk().await;
    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .open()
        .await
        .unwrap();

    let mut sub = broker.subscribe_notifications();

    let added = broker.triggers_add(TriggersAdd {
        filter: "l>=ERROR".into(),
        ..Default::default()
    }).await.unwrap();

    daemon.inject_log("error", "fire it").await;

    match tokio::time::timeout(std::time::Duration::from_secs(2), sub.recv()).await {
        Ok(Ok(Notification::TriggerFired(payload))) => {
            assert_eq!(payload.trigger_id, added.id);
            assert_eq!(payload.matched_entry.message, "fire it");
        }
        other => panic!("expected TriggerFired, got: {:?}", other),
    }
}
```

```
cargo test -p logmon-broker-sdk --test notifications
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(sdk): typed Notification enum (TriggerFired with full payload, Reconnected variant for reconnect events)"
```

---

### Task 15: Filter builder

**Files:**
- Create: `crates/sdk/src/filter.rs`
- Modify: `crates/sdk/src/lib.rs` — re-export
- Test: `crates/sdk/tests/filter_builder.rs` (new)

- [ ] **Step 1: Write the failing test**

```rust
// crates/sdk/tests/filter_builder.rs
use logmon_broker_sdk::{Filter, Level, SpanStatus, SpanKind};

#[test]
fn level_at_least() {
    let f = Filter::builder().level_at_least(Level::Error).build();
    assert_eq!(f, "l>=ERROR");
}

#[test]
fn multiple_qualifiers_comma_joined() {
    let f = Filter::builder()
        .level_at_least(Level::Error)
        .field_eq_in_message("started", false)
        .build();
    assert!(f.contains("l>=ERROR"));
    assert!(f.contains("m=started"));
    assert!(f.contains(", "));
}

#[test]
fn regex_with_case_insensitive() {
    let f = Filter::builder()
        .pattern_regex("/panic|unwrap/", true)
        .build();
    assert_eq!(f, "/panic|unwrap/i");
}

#[test]
fn additional_field_substring() {
    let f = Filter::builder()
        .additional_field("_test_id", "run_abc")
        .build();
    assert_eq!(f, "_test_id=run_abc");
}

#[test]
fn span_status_typed_enum() {
    let f = Filter::builder()
        .span_status(SpanStatus::Error)
        .build();
    assert_eq!(f, "st=error");
}

#[test]
fn duration_at_least() {
    let f = Filter::builder().duration_at_least_ms(100).build();
    assert_eq!(f, "d>=100");
}

#[test]
fn bookmark_after() {
    let f = Filter::builder().bookmark_after("test_start").build();
    assert_eq!(f, "b>=test_start");
}

#[test]
fn match_all() {
    let f = Filter::builder().match_all().build();
    assert_eq!(f, "ALL");
}

#[test]
fn quoted_value_with_comma() {
    let f = Filter::builder()
        .message("hello, world")
        .build();
    assert_eq!(f, "m=\"hello, world\"");
}

// Integration test against real broker (validates parser accepts SDK output)
#[tokio::test]
async fn builder_strings_parse_in_broker() {
    use logmon_broker_sdk::Broker;
    use logmon_broker_protocol::LogsRecent;
    use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;

    let daemon = spawn_test_daemon_for_sdk().await;
    let broker = Broker::connect().socket_path(daemon.socket_path.clone()).open().await.unwrap();

    daemon.inject_log("error", "first").await;
    daemon.inject_log("info", "second").await;

    let f = Filter::builder().level_at_least(Level::Error).build();
    let r = broker.logs_recent(LogsRecent { filter: Some(f), ..Default::default() }).await.unwrap();
    assert_eq!(r.count, 1);
    assert_eq!(r.logs[0].message, "first");
}
```

- [ ] **Step 2: Run test, expect fail**

```
cargo test -p logmon-broker-sdk --test filter_builder
```

Expected: FAIL — `Filter`, `Level`, `SpanStatus`, `SpanKind` not defined.

- [ ] **Step 3: Author filter.rs**

```rust
// crates/sdk/src/filter.rs
use std::fmt::Write;

#[derive(Debug, Clone, Copy)]
pub enum Level { Emergency, Alert, Critical, Error, Warning, Notice, Info, Debug, Trace }

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Emergency => "EMERGENCY",
            Level::Alert     => "ALERT",
            Level::Critical  => "CRITICAL",
            Level::Error     => "ERROR",
            Level::Warning   => "WARNING",
            Level::Notice    => "NOTICE",
            Level::Info      => "INFO",
            Level::Debug     => "DEBUG",
            Level::Trace     => "TRACE",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SpanStatus { Ok, Error, Unset }
impl SpanStatus {
    fn as_str(self) -> &'static str {
        match self { SpanStatus::Ok => "ok", SpanStatus::Error => "error", SpanStatus::Unset => "unset" }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SpanKind { Server, Client, Producer, Consumer, Internal }
impl SpanKind {
    fn as_str(self) -> &'static str {
        match self {
            SpanKind::Server   => "server",
            SpanKind::Client   => "client",
            SpanKind::Producer => "producer",
            SpanKind::Consumer => "consumer",
            SpanKind::Internal => "internal",
        }
    }
}

pub struct Filter;

impl Filter {
    pub fn builder() -> FilterBuilder {
        FilterBuilder { qualifiers: Vec::new() }
    }
}

pub struct FilterBuilder {
    qualifiers: Vec<String>,
}

fn esc(value: &str) -> String {
    if value.contains(',') || value.contains('"') {
        let escaped = value.replace('"', "\\\"");
        format!("\"{}\"", escaped)
    } else {
        value.to_string()
    }
}

fn regex_lit(pattern: &str, case_insensitive: bool) -> String {
    let body = pattern.trim_matches('/');
    if case_insensitive {
        format!("/{body}/i")
    } else {
        format!("/{body}/")
    }
}

impl FilterBuilder {
    pub fn build(self) -> String {
        if self.qualifiers.is_empty() { return String::new(); }
        self.qualifiers.join(", ")
    }

    pub fn match_all(mut self) -> Self    { self.qualifiers.push("ALL".into()); self }
    pub fn match_none(mut self) -> Self   { self.qualifiers.push("NONE".into()); self }

    // Level
    pub fn level_at_least(mut self, l: Level) -> Self { self.qualifiers.push(format!("l>={}", l.as_str())); self }
    pub fn level_at_most(mut self, l: Level)  -> Self { self.qualifiers.push(format!("l<={}", l.as_str())); self }
    pub fn level_eq(mut self, l: Level)       -> Self { self.qualifiers.push(format!("l={}", l.as_str())); self }

    // Bare pattern
    pub fn pattern(mut self, sub: &str) -> Self { self.qualifiers.push(esc(sub)); self }
    pub fn pattern_regex(mut self, regex: &str, case_insensitive: bool) -> Self {
        self.qualifiers.push(regex_lit(regex, case_insensitive)); self
    }

    // Log selectors — each has substring + regex variants
    pub fn message(mut self, sub: &str) -> Self        { self.qualifiers.push(format!("m={}", esc(sub))); self }
    pub fn message_regex(mut self, r: &str, ci: bool) -> Self { self.qualifiers.push(format!("m={}", regex_lit(r, ci))); self }
    pub fn full_message(mut self, sub: &str) -> Self   { self.qualifiers.push(format!("fm={}", esc(sub))); self }
    pub fn full_message_regex(mut self, r: &str, ci: bool) -> Self { self.qualifiers.push(format!("fm={}", regex_lit(r, ci))); self }
    pub fn message_or_full(mut self, sub: &str) -> Self    { self.qualifiers.push(format!("mfm={}", esc(sub))); self }
    pub fn message_or_full_regex(mut self, r: &str, ci: bool) -> Self { self.qualifiers.push(format!("mfm={}", regex_lit(r, ci))); self }
    pub fn host(mut self, sub: &str) -> Self           { self.qualifiers.push(format!("h={}", esc(sub))); self }
    pub fn host_regex(mut self, r: &str, ci: bool) -> Self  { self.qualifiers.push(format!("h={}", regex_lit(r, ci))); self }
    pub fn facility(mut self, sub: &str) -> Self       { self.qualifiers.push(format!("fa={}", esc(sub))); self }
    pub fn facility_regex(mut self, r: &str, ci: bool) -> Self { self.qualifiers.push(format!("fa={}", regex_lit(r, ci))); self }
    pub fn file(mut self, sub: &str) -> Self           { self.qualifiers.push(format!("fi={}", esc(sub))); self }
    pub fn file_regex(mut self, r: &str, ci: bool) -> Self { self.qualifiers.push(format!("fi={}", regex_lit(r, ci))); self }
    pub fn line(mut self, n: u32) -> Self              { self.qualifiers.push(format!("ln={}", n)); self }

    // Span selectors
    pub fn span_name(mut self, sub: &str) -> Self      { self.qualifiers.push(format!("sn={}", esc(sub))); self }
    pub fn span_name_regex(mut self, r: &str, ci: bool) -> Self { self.qualifiers.push(format!("sn={}", regex_lit(r, ci))); self }
    pub fn service(mut self, sub: &str) -> Self        { self.qualifiers.push(format!("sv={}", esc(sub))); self }
    pub fn service_regex(mut self, r: &str, ci: bool) -> Self { self.qualifiers.push(format!("sv={}", regex_lit(r, ci))); self }
    pub fn span_status(mut self, s: SpanStatus) -> Self { self.qualifiers.push(format!("st={}", s.as_str())); self }
    pub fn span_kind(mut self, k: SpanKind) -> Self    { self.qualifiers.push(format!("sk={}", k.as_str())); self }
    pub fn duration_at_least_ms(mut self, ms: u32) -> Self { self.qualifiers.push(format!("d>={}", ms)); self }
    pub fn duration_at_most_ms(mut self, ms: u32) -> Self  { self.qualifiers.push(format!("d<={}", ms)); self }

    // Bookmarks
    pub fn bookmark_after(mut self, name: &str) -> Self  { self.qualifiers.push(format!("b>={}", name)); self }
    pub fn bookmark_before(mut self, name: &str) -> Self { self.qualifiers.push(format!("b<={}", name)); self }

    // Escape hatch
    pub fn additional_field(mut self, name: &str, value: &str) -> Self {
        self.qualifiers.push(format!("{name}={}", esc(value))); self
    }
    pub fn additional_field_regex(mut self, name: &str, regex: &str, ci: bool) -> Self {
        self.qualifiers.push(format!("{name}={}", regex_lit(regex, ci))); self
    }
}
```

Re-export from `crates/sdk/src/lib.rs`:

```rust
pub mod filter;
pub use filter::{Filter, FilterBuilder, Level, SpanStatus, SpanKind};
```

I omitted one helper used in tests — let me sync: `field_eq_in_message` was placeholder. Use `.message(...)` instead. Update test accordingly.

- [ ] **Step 4: Run test, expect pass**

```
cargo test -p logmon-broker-sdk --test filter_builder
```

Expected: ALL PASS, including `builder_strings_parse_in_broker` integration test.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(sdk): typed filter builder — methods per selector, typed Level/SpanStatus/SpanKind enums"
```

---

### Task 16: Reconnection state machine (named-session resume)

**Files:**
- Create: `crates/sdk/src/reconnect.rs`
- Modify: `crates/sdk/src/connect.rs` — replace `Mutex<DaemonBridge>` with reconnect-aware connection state
- Test: `crates/sdk/tests/reconnect.rs`

This is the densest task. Read it through before implementing.

- [ ] **Step 1: Define connection state**

```rust
// crates/sdk/src/reconnect.rs
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, broadcast};
use tokio::net::UnixStream;

use crate::bridge::DaemonBridge;
use crate::{BrokerError, Notification};
use logmon_broker_protocol::*;

pub(crate) enum ConnectionState {
    Connected(DaemonBridge),
    Reconnecting,
    PermanentlyFailed(BrokerError),
}

pub(crate) struct ConnectionManager {
    pub state: Mutex<ConnectionState>,
    pub state_changed: Notify,
    pub notification_tx: broadcast::Sender<Notification>,
    pub config: Arc<crate::BrokerBuilder>,
    pub session_name: Option<String>,
    pub client_info: Option<serde_json::Value>,
}

impl ConnectionManager {
    pub async fn current_bridge(&self, deadline: tokio::time::Instant) -> Result<DaemonBridge, BrokerError> {
        loop {
            let state = self.state.lock().await;
            match &*state {
                ConnectionState::Connected(bridge) => return Ok(bridge.clone()),
                ConnectionState::Reconnecting => {
                    drop(state);
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    tokio::select! {
                        _ = self.state_changed.notified() => continue,
                        _ = tokio::time::sleep(remaining) => return Err(BrokerError::Disconnected),
                    }
                }
                ConnectionState::PermanentlyFailed(e) => return Err(e.clone()),
            }
        }
    }

    pub async fn enter_reconnecting(self: Arc<Self>) {
        {
            let mut state = self.state.lock().await;
            if matches!(*state, ConnectionState::Reconnecting | ConnectionState::PermanentlyFailed(_)) {
                return;
            }
            *state = ConnectionState::Reconnecting;
        }
        self.state_changed.notify_waiters();

        // Spawn reconnect task
        tokio::spawn(reconnect_loop(self));
    }
}

async fn reconnect_loop(mgr: Arc<ConnectionManager>) {
    let mut backoff = mgr.config.reconnect_initial_backoff;
    let max_backoff = mgr.config.reconnect_max_backoff;
    let max_attempts = mgr.config.reconnect_max_attempts;

    for attempt in 0..max_attempts {
        match try_handshake(&mgr).await {
            Ok((bridge, is_new)) => {
                if is_new {
                    // Resurrection — fail loud as SessionLost
                    let mut state = mgr.state.lock().await;
                    *state = ConnectionState::PermanentlyFailed(BrokerError::SessionLost);
                    mgr.state_changed.notify_waiters();
                    // Drop notification_tx to close subscriber channels
                    return;
                }
                // Successful resume
                let mut state = mgr.state.lock().await;
                *state = ConnectionState::Connected(bridge);
                mgr.state_changed.notify_waiters();
                drop(state);
                let _ = mgr.notification_tx.send(Notification::Reconnected);
                // Drained queue notifications follow naturally — daemon sends them on the new connection
                return;
            }
            Err(e) => {
                tracing::warn!(attempt, error=%e, "reconnect attempt failed; backing off");
                let jitter = rand::random::<f32>() * 0.3 + 0.85;  // ±15%
                let sleep_dur = Duration::from_secs_f32(backoff.as_secs_f32() * jitter);
                tokio::time::sleep(sleep_dur).await;
                backoff = std::cmp::min(backoff * 2, max_backoff);
            }
        }
    }

    // Exhausted — PermanentlyFailed with Disconnected
    let mut state = mgr.state.lock().await;
    *state = ConnectionState::PermanentlyFailed(BrokerError::Disconnected);
    mgr.state_changed.notify_waiters();
}

async fn try_handshake(mgr: &Arc<ConnectionManager>) -> Result<(DaemonBridge, bool), BrokerError> {
    let socket_path = mgr.config.socket_path.clone()
        .unwrap_or_else(crate::connect::default_socket_path);

    #[cfg(unix)]
    let stream = UnixStream::connect(&socket_path).await?;
    #[cfg(windows)]
    let stream = tokio::net::TcpStream::connect("127.0.0.1:12200").await?;

    let bridge = DaemonBridge::spawn(stream, mgr.notification_tx.clone()).await?;

    let params = SessionStartParams {
        name: mgr.session_name.clone(),
        protocol_version: PROTOCOL_VERSION,
        client_info: mgr.client_info.clone(),
    };
    let result_value = bridge.call("session.start", serde_json::to_value(&params).unwrap()).await
        .map_err(|e| BrokerError::Protocol(e.to_string()))?;
    let result: SessionStartResult = serde_json::from_value(result_value)
        .map_err(|e| BrokerError::Protocol(e.to_string()))?;

    Ok((bridge, result.is_new))
}
```

Add `rand = "0.8"` to `crates/sdk/Cargo.toml`.

- [ ] **Step 2: Refactor Broker to use ConnectionManager**

In `crates/sdk/src/connect.rs`:

```rust
pub struct Broker {
    pub(crate) inner: Arc<crate::reconnect::ConnectionManager>,
}

impl Broker {
    pub async fn call_typed<P, R>(&self, method: &str, params: P) -> Result<R, BrokerError>
    where
        P: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let value = serde_json::to_value(&params)
            .map_err(|e| BrokerError::Protocol(e.to_string()))?;
        let deadline = tokio::time::Instant::now() + self.inner.config.call_timeout;
        let bridge = self.inner.current_bridge(deadline).await?;
        match bridge.call(method, value).await {
            Ok(v) => serde_json::from_value(v)
                .map_err(|e| BrokerError::Protocol(e.to_string())),
            Err(e) => {
                // Distinguish: was this a transport error (trigger reconnect)
                // or a method-level error (return as Method)?
                if is_transport_error(&e) {
                    self.inner.clone().enter_reconnecting().await;
                    Err(BrokerError::Disconnected)
                } else {
                    Err(map_method_error(e))
                }
            }
        }
    }
}
```

`DaemonBridge` needs to expose enough error context for the `is_transport_error` discriminator — refactor `bridge.rs` to use a typed `BridgeError` enum (Transport / Rpc { code, message }).

- [ ] **Step 3: Wire up the bridge to detect EOF and trigger enter_reconnecting**

In `crates/sdk/src/bridge.rs`, the existing reader loop currently `break`s on EOF. Change it to invoke a callback (passed as Arc<ConnectionManager>) that calls `enter_reconnecting`:

```rust
pub fn spawn_reader_loop(
    bridge: Arc<DaemonBridge>,
    reader: Box<dyn tokio::io::AsyncBufRead + Unpin + Send>,
    on_disconnect: Box<dyn FnOnce() + Send + 'static>,
) {
    tokio::spawn(async move {
        let mut reader = reader;
        loop {
            match transport::read_daemon_message(&mut reader).await {
                Ok(Some(DaemonMessage::Response(resp))) => { /* dispatch */ }
                Ok(Some(DaemonMessage::Notification(notif))) => { /* convert + broadcast */ }
                Ok(None) | Err(_) => { on_disconnect(); break; }
            }
        }
    });
}
```

The `on_disconnect` callback is constructed in the connect path:

```rust
let mgr_clone = mgr.clone();
let on_disconnect: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
    let mgr = mgr_clone.clone();
    tokio::spawn(async move { mgr.enter_reconnecting().await; });
});
```

- [ ] **Step 4: Tests for reconnect**

```rust
// crates/sdk/tests/reconnect.rs
use logmon_broker_sdk::{Broker, BrokerError, Notification};
use logmon_broker_protocol::{TriggersAdd, TriggersList, TriggersListResult};
use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;

#[tokio::test]
async fn named_session_resumes_across_daemon_restart() {
    let mut daemon = spawn_test_daemon_for_sdk().await;

    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .session_name("resume_test")
        .open()
        .await
        .unwrap();

    let added = broker.triggers_add(TriggersAdd {
        filter: "l>=ERROR".into(),
        ..Default::default()
    }).await.unwrap();

    let mut sub = broker.subscribe_notifications();

    // Restart the daemon — preserves state.json
    daemon.restart().await;

    // Expect Reconnected on the subscription
    match tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv()).await {
        Ok(Ok(Notification::Reconnected)) => {}
        other => panic!("expected Reconnected; got {other:?}"),
    }

    // Trigger should still be present
    let list: TriggersListResult = broker.triggers_list(TriggersList {}).await.unwrap();
    assert!(list.triggers.iter().any(|t| t.id == added.id),
        "trigger {} should survive restart; got {:?}", added.id, list.triggers);
}

#[tokio::test]
async fn anonymous_session_reconnect_fails_with_session_lost() {
    let mut daemon = spawn_test_daemon_for_sdk().await;

    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        // No session_name → anonymous
        .open()
        .await
        .unwrap();

    daemon.restart().await;

    // Calls error with SessionLost
    let result = broker.triggers_list(TriggersList {}).await;
    match result {
        Err(BrokerError::SessionLost) => {}
        other => panic!("expected SessionLost; got {other:?}"),
    }
}

#[tokio::test]
async fn resurrection_treated_as_session_lost() {
    let mut daemon = spawn_test_daemon_for_sdk().await;

    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .session_name("resurrect_test")
        .open()
        .await
        .unwrap();

    // Wipe state.json mid-flight then restart
    daemon.wipe_state_and_restart().await;

    let result = broker.triggers_list(TriggersList {}).await;
    match result {
        Err(BrokerError::SessionLost) => {}
        other => panic!("expected SessionLost on resurrection; got {other:?}"),
    }
}
```

- [ ] **Step 5: Run tests, expect pass**

```
cargo test -p logmon-broker-sdk --test reconnect
```

Expected: PASS for all three.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(sdk): named-session reconnect with exponential backoff; resurrection ⇒ SessionLost; anonymous ⇒ SessionLost"
```

---

### Task 17: Notification ordering across reconnect

**Files:**
- Modify: `crates/sdk/src/reconnect.rs` — emit Reconnected before draining queue notifications
- Test: `crates/sdk/tests/reconnect_ordering.rs`

- [ ] **Step 1: Write the failing test**

```rust
// crates/sdk/tests/reconnect_ordering.rs
use logmon_broker_sdk::{Broker, Notification};
use logmon_broker_protocol::TriggersAdd;
use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;

#[tokio::test]
async fn reconnect_then_drained_then_live() {
    let mut daemon = spawn_test_daemon_for_sdk().await;

    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .session_name("order_test")
        .open()
        .await
        .unwrap();

    let added = broker.triggers_add(TriggersAdd { filter: "l>=ERROR".into(), ..Default::default() }).await.unwrap();
    let mut sub = broker.subscribe_notifications();

    // Disconnect the broker without removing the socket yet — generate a queued notification
    daemon.pause_accept().await;
    daemon.inject_log("error", "queued").await;       // notification queues for resume_test session
    // Now restart to force reconnect
    daemon.resume_accept().await;
    // … cycle: kill + restart preserving state.json

    // Expect: Reconnected, then TriggerFired("queued"), then live
    match sub.recv().await.unwrap() {
        Notification::Reconnected => {}
        other => panic!("first event must be Reconnected; got {other:?}"),
    }
    match sub.recv().await.unwrap() {
        Notification::TriggerFired(p) => {
            assert_eq!(p.matched_entry.message, "queued");
            assert_eq!(p.trigger_id, added.id);
        }
        other => panic!("second event must be TriggerFired(queued); got {other:?}"),
    }

    // Live — fire one more
    daemon.inject_log("error", "live").await;
    match sub.recv().await.unwrap() {
        Notification::TriggerFired(p) => {
            assert_eq!(p.matched_entry.message, "live");
        }
        other => panic!("third event must be TriggerFired(live); got {other:?}"),
    }
}
```

- [ ] **Step 2: Verify implementation matches**

The implementation in Task 16 emits `Reconnected` immediately after handshake success but before the daemon's drained-queue notifications arrive on the wire. Verify by reading the reconnect_loop code: yes, the order is `state.replace → notify_waiters → notification_tx.send(Reconnected)`. Daemon's drained notifications then arrive on the new bridge's reader loop, are converted to `Notification::TriggerFired(...)`, and broadcast — necessarily after the `Reconnected` event since the bridge wasn't even created yet at the moment Reconnected was sent.

- [ ] **Step 3: Run test, expect pass**

```
cargo test -p logmon-broker-sdk --test reconnect_ordering
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "test(sdk): assert Reconnected → drained → live notification ordering"
```

---

## Phase 5 — Binary lifecycle + system service

### Task 18: Broker `status` subcommand

**Files:**
- Modify: `crates/broker/src/main.rs` — implement Status arm

- [ ] **Step 1: Implement status**

```rust
fn status() -> anyhow::Result<i32> {
    use logmon_broker_core::daemon::persistence::config_dir;
    let dir = config_dir();
    let pid_path = dir.join("daemon.pid");
    let socket_path = dir.join("logmon.sock");

    if !pid_path.exists() {
        println!("not running");
        return Ok(1);
    }
    let pid_str = std::fs::read_to_string(&pid_path)?;
    let pid: u32 = pid_str.trim().parse()?;
    if !is_process_alive(pid) {
        println!("not running (stale pid file)");
        return Ok(1);
    }
    println!("running pid={pid} socket={}", socket_path.display());
    Ok(0)
}

fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    return std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    #[cfg(windows)]
    return std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
}
```

Wire up in main:

```rust
match cli.command {
    Some(Subcommand::Status) => {
        std::process::exit(status()?);
    }
    // ...
}
```

- [ ] **Step 2: Smoke test**

```
target/debug/logmon-broker status
```

Expected: when no daemon running, prints "not running" and exits 1.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat(broker): status subcommand reports running pid + socket"
```

---

### Task 19: SIGTERM + SIGINT graceful shutdown

**Files:**
- Modify: `crates/broker/src/server.rs` — replace `tokio::signal::ctrl_c()` with select on both SIGTERM and SIGINT

- [ ] **Step 1: Update accept loop**

```rust
use tokio::signal::unix::{signal, SignalKind};

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut intr = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => tracing::info!("received SIGTERM"),
            _ = intr.recv() => tracing::info!("received SIGINT"),
        }
    }
    #[cfg(windows)]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
```

In the existing accept loop, replace:

```rust
_ = tokio::signal::ctrl_c() => { /* … */ }
```

with:

```rust
_ = wait_for_shutdown() => {
    tracing::info!("shutdown requested");
    break;
}
```

- [ ] **Step 2: Smoke test**

Start broker:

```
target/debug/logmon-broker &
sleep 1
kill -TERM $!
wait $!
```

Expected: broker exits 0; socket file removed; daemon.log records "received SIGTERM".

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat(broker): SIGTERM and SIGINT both trigger graceful shutdown"
```

---

### Task 20: sd-notify `READY=1` emit

**Files:**
- Modify: `crates/broker/Cargo.toml` — sd-notify dep
- Modify: `crates/broker/src/server.rs` — emit READY=1 after receivers bound

- [ ] **Step 1: Add dependency**

`crates/broker/Cargo.toml`:

```toml
[target.'cfg(target_os = "linux")'.dependencies]
sd-notify = { workspace = true }
```

- [ ] **Step 2: Emit READY=1 at the right moment**

In `crates/broker/src/server.rs::run_daemon`, after receivers are started and before the accept loop:

```rust
// existing receivers startup ...
all_receivers_info.extend(otlp_info);
send_otel_beacon("OTEL:ONLINE\n");

// systemd ready notification
#[cfg(target_os = "linux")]
{
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error=%e, "sd_notify ready failed (likely not running under systemd)");
    }
}

info!("daemon ready, listening for connections");
// existing accept loop ...
```

- [ ] **Step 3: Smoke test**

On Linux, run a manual test:

```
systemd-run --user --service-type=notify ./target/debug/logmon-broker
```

Expected: `systemctl --user status run-XXXX.service` shows "active (running)" within 1 second of start.

On macOS: skip; ensure cargo build doesn't break.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(broker): emit systemd READY=1 after receivers bound (Type=notify support)"
```

---

### Task 21: Stale PID self-clean at broker startup

**Files:**
- Modify: `crates/broker/src/server.rs` — at startup, check existing PID file and clean if stale

- [ ] **Step 1: Implement stale-cleanup**

In `run_daemon`, before binding the socket:

```rust
let pid_path = dir.join("daemon.pid");
if pid_path.exists() {
    let pid_str = std::fs::read_to_string(&pid_path).unwrap_or_default();
    if let Ok(pid) = pid_str.trim().parse::<u32>() {
        if is_process_alive(pid) {
            anyhow::bail!("another broker is already running (pid {pid}); abort");
        }
    }
    tracing::info!("removing stale pid file from previous run");
    let _ = std::fs::remove_file(&pid_path);
    let _ = std::fs::remove_file(dir.join("logmon.sock"));
}
```

`is_process_alive` already lives in main.rs (Task 18). Move it to `crates/broker/src/lib.rs` (yes, even broker crate gets a tiny lib.rs for sharing) or just duplicate — it's 10 lines. Lift into a small `crates/broker/src/process.rs` and reuse from both places.

- [ ] **Step 2: Smoke test**

```
target/debug/logmon-broker &
PID1=$!
sleep 1
kill -9 $PID1   # leave stale pid file
target/debug/logmon-broker &
PID2=$!
sleep 1
kill -TERM $PID2
wait
```

Expected: second start succeeds; daemon.log shows "removing stale pid file from previous run".

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat(broker): self-clean stale pid + socket at startup"
```

---

### Task 22: Shim broker discovery — env / PATH / sibling

**Files:** Already done in Task 5 Step 6.

- [ ] **Step 1: Verify**

Re-read `crates/mcp/src/auto_start.rs::locate_broker_binary` — confirm the three-tier lookup is in place. If anything's missing, complete it now.

- [ ] **Step 2: Smoke test**

```
unset LOGMON_BROKER_BIN
PATH= target/debug/logmon-mcp --session test </dev/null 2>&1 | head -5
```

Expected: clear error message mentioning all three paths and `cargo install --path crates/broker`.

```
LOGMON_BROKER_BIN=$(pwd)/target/debug/logmon-broker target/debug/logmon-mcp --session test
```

Expected: shim spawns broker successfully.

- [ ] **Step 3: Commit (only if changes made)**

```bash
git diff --quiet || git commit -am "feat(mcp): three-tier broker discovery — env / PATH / sibling"
```

---

### Task 23: launchd plist template + install-service on macOS

**Files:**
- Create: `crates/broker/templates/launchd.plist.template`
- Create: `crates/broker/src/service/mod.rs`
- Create: `crates/broker/src/service/macos.rs`
- Modify: `crates/broker/src/main.rs` — wire up install-service / uninstall-service

- [ ] **Step 1: Author the plist template**

```xml
<!-- crates/broker/templates/launchd.plist.template -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>            <string>logmon.broker</string>
    <key>ProgramArguments</key> <array><string>{BINARY_PATH}</string></array>
    <key>RunAtLoad</key>        <true/>
    <key>KeepAlive</key>        <true/>
</dict>
</plist>
```

- [ ] **Step 2: Author service::macos**

```rust
// crates/broker/src/service/macos.rs
use std::path::PathBuf;
use anyhow::{Result, Context, bail};

const PLIST_TEMPLATE: &str = include_str!("../../templates/launchd.plist.template");
const LABEL: &str = "logmon.broker";

pub fn install(scope: Scope) -> Result<()> {
    let exe = std::env::current_exe().context("resolve current_exe")?;
    let plist_path = scope.plist_path()?;
    let rendered = PLIST_TEMPLATE.replace("{BINARY_PATH}", exe.to_str().unwrap());

    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&plist_path, rendered)?;

    // bootout existing first (idempotent)
    let _ = bootout(scope);
    // bootstrap
    let target = scope.bootstrap_target();
    let status = std::process::Command::new("launchctl")
        .args(["bootstrap", &target, plist_path.to_str().unwrap()])
        .status()?;
    if !status.success() {
        bail!("launchctl bootstrap failed");
    }
    println!("installed and started: {}", plist_path.display());
    Ok(())
}

pub fn uninstall(scope: Scope) -> Result<()> {
    let plist_path = scope.plist_path()?;
    let _ = bootout(scope);
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)?;
        println!("removed {}", plist_path.display());
    } else {
        println!("not installed (no-op)");
    }
    Ok(())
}

fn bootout(scope: Scope) -> Result<()> {
    let target = scope.bootstrap_target();
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{target}/{LABEL}")])
        .status();
    Ok(())
}

#[derive(Copy, Clone, Debug)]
pub enum Scope { User, System }

impl Scope {
    fn plist_path(&self) -> Result<PathBuf> {
        match self {
            Scope::User => {
                let home = dirs::home_dir().context("no home dir")?;
                Ok(home.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
            }
            Scope::System => Ok(PathBuf::from(format!("/Library/LaunchDaemons/{LABEL}.plist"))),
        }
    }
    fn bootstrap_target(&self) -> String {
        match self {
            Scope::User   => format!("gui/{}", users::get_current_uid()),
            Scope::System => "system".into(),
        }
    }
}
```

Add `dirs = "5"`, `users = "0.11"` to broker dependencies (target = macos).

- [ ] **Step 3: Author service::mod.rs**

```rust
// crates/broker/src/service/mod.rs
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;

use anyhow::{Result, bail};

#[derive(Copy, Clone, Debug)]
pub enum Scope { User, System }

#[cfg(target_os = "macos")]
impl From<Scope> for macos::Scope {
    fn from(s: Scope) -> Self { match s { Scope::User => macos::Scope::User, Scope::System => macos::Scope::System } }
}

pub fn install(scope: Scope) -> Result<()> {
    #[cfg(target_os = "macos")]
    return macos::install(scope.into());
    #[cfg(target_os = "linux")]
    return linux::install(scope.into());
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("install-service is only supported on macOS and Linux");
}

pub fn uninstall(scope: Scope) -> Result<()> {
    #[cfg(target_os = "macos")]
    return macos::uninstall(scope.into());
    #[cfg(target_os = "linux")]
    return linux::uninstall(scope.into());
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("install-service is only supported on macOS and Linux");
}
```

- [ ] **Step 4: Wire up subcommands in main.rs**

```rust
#[derive(Subcommand)]
enum Subcommand {
    Status,
    InstallService {
        #[arg(long, default_value = "user")]
        scope: ScopeArg,
    },
    UninstallService {
        #[arg(long, default_value = "user")]
        scope: ScopeArg,
    },
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum ScopeArg { User, System }

impl From<ScopeArg> for service::Scope {
    fn from(s: ScopeArg) -> Self { match s { ScopeArg::User => service::Scope::User, ScopeArg::System => service::Scope::System } }
}

// In main():
Some(Subcommand::InstallService { scope }) => service::install(scope.into())?,
Some(Subcommand::UninstallService { scope }) => service::uninstall(scope.into())?,
```

Adjust the main struct to use clap with `--user` / `--system` flags rather than ValueEnum if preferred — both work. Above is the simpler form.

Spec says CLI shape is `install-service [--user|--system]`. clap's `--scope=user|system` is close but not identical. Adjust to use a boolean `--user` (default) vs `--system` flag if exact CLI shape matters.

- [ ] **Step 5: Smoke test (macOS)**

```
target/debug/logmon-broker install-service --scope user
launchctl print gui/$(id -u)/logmon.broker | head -5
target/debug/logmon-broker status
target/debug/logmon-broker uninstall-service --scope user
```

Expected: install succeeds → launchctl shows the agent → status reports running → uninstall removes it.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(broker): install-service / uninstall-service on macOS via launchd"
```

---

### Task 24: systemd unit template + install-service on Linux

**Files:**
- Create: `crates/broker/templates/systemd.service.template`
- Create: `crates/broker/src/service/linux.rs`

- [ ] **Step 1: Author template**

```ini
; crates/broker/templates/systemd.service.template
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

- [ ] **Step 2: Author service::linux**

```rust
// crates/broker/src/service/linux.rs
use std::path::PathBuf;
use anyhow::{Result, Context, bail};

const UNIT_TEMPLATE: &str = include_str!("../../templates/systemd.service.template");
const UNIT_NAME: &str = "logmon-broker.service";

#[derive(Copy, Clone, Debug)]
pub enum Scope { User, System }

pub fn install(scope: Scope) -> Result<()> {
    let exe = std::env::current_exe().context("resolve current_exe")?;
    let unit_path = scope.unit_path()?;
    let rendered = UNIT_TEMPLATE.replace("{BINARY_PATH}", exe.to_str().unwrap());

    if let Some(parent) = unit_path.parent() { std::fs::create_dir_all(parent)?; }
    std::fs::write(&unit_path, rendered)?;

    systemctl(scope, &["daemon-reload"])?;
    systemctl(scope, &["enable", "--now", UNIT_NAME])?;
    println!("installed and started: {}", unit_path.display());
    Ok(())
}

pub fn uninstall(scope: Scope) -> Result<()> {
    let unit_path = scope.unit_path()?;
    let _ = systemctl(scope, &["disable", "--now", UNIT_NAME]);
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)?;
    }
    let _ = systemctl(scope, &["daemon-reload"]);
    println!("removed {}", unit_path.display());
    Ok(())
}

fn systemctl(scope: Scope, args: &[&str]) -> Result<()> {
    let mut cmd = std::process::Command::new("systemctl");
    if matches!(scope, Scope::User) { cmd.arg("--user"); }
    cmd.args(args);
    let status = cmd.status().context("invoke systemctl")?;
    if !status.success() { bail!("systemctl {args:?} failed"); }
    Ok(())
}

impl Scope {
    fn unit_path(&self) -> Result<PathBuf> {
        match self {
            Scope::User => {
                let home = dirs::home_dir().context("no home dir")?;
                Ok(home.join(".config/systemd/user").join(UNIT_NAME))
            }
            Scope::System => Ok(PathBuf::from(format!("/etc/systemd/system/{UNIT_NAME}"))),
        }
    }
}
```

- [ ] **Step 3: Wire scope conversion in service/mod.rs**

```rust
#[cfg(target_os = "linux")]
impl From<Scope> for linux::Scope {
    fn from(s: Scope) -> Self { match s { Scope::User => linux::Scope::User, Scope::System => linux::Scope::System } }
}
```

- [ ] **Step 4: Smoke test (Linux)**

```
./target/debug/logmon-broker install-service --scope user
systemctl --user status logmon-broker
./target/debug/logmon-broker status
./target/debug/logmon-broker uninstall-service --scope user
```

Expected: install succeeds → systemctl shows active (running) within 2 s of start → status reports running → uninstall removes.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(broker): install-service / uninstall-service on Linux via systemd Type=notify"
```

---

### Task 25: Manual smoke-test checklist in README

**Files:**
- Modify: `README.md` — add a "Manual smoke tests" section

- [ ] **Step 1: Add section**

Append to README.md (or `docs/dev.md` if more appropriate):

```markdown
## Manual smoke tests

These are not run in CI; verify locally before tagging a release.

### Build + binaries
```
cargo build --workspace
ls -lh target/debug/logmon-broker target/debug/logmon-mcp
```

### Auto-start path (legacy)
```
target/debug/logmon-mcp --session smoke <<'EOF'
EOF
```
Expected: shim exits cleanly; daemon.pid + logmon.sock created in ~/.config/logmon/.

### System service install (macOS)
```
target/debug/logmon-broker install-service --scope user
launchctl print gui/$(id -u)/logmon.broker | head -5
target/debug/logmon-broker status
target/debug/logmon-broker uninstall-service --scope user
```

### System service install (Linux)
```
target/debug/logmon-broker install-service --scope user
systemctl --user status logmon-broker
target/debug/logmon-broker status
target/debug/logmon-broker uninstall-service --scope user
```
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: add manual smoke-test checklist for binaries + system-service install"
```

---

## Phase 6 — Documentation + skill updates

### Task 26: Update logmon-mcp README.md

**Files:**
- Modify: `/Users/yuval/Documents/Projects/MCPs/logmon-mcp/README.md`

- [ ] **Step 1: Rewrite architecture section**

Replace the existing daemon+shim diagram with the workspace + two-binary diagram:

```markdown
## Architecture

logmon is a **broker** for structured logs and OTLP traces. It runs as a long-lived daemon (`logmon-broker`) that ingests via GELF (UDP+TCP) and OTLP (gRPC+HTTP), and serves multiple clients over a Unix domain socket using JSON-RPC 2.0.

**Binaries:**
- `logmon-broker` — the daemon. Run as a system service (launchd / systemd) for always-on availability.
- `logmon-mcp` — MCP shim binary. Exposes broker tools to Claude Code / other MCP hosts. Auto-starts the broker if not already running.

**Other clients:** anything that depends on the public `logmon-broker-sdk` Rust crate, or speaks JSON-RPC against the documented protocol (see `crates/protocol/protocol-v1.schema.json`).
```

- [ ] **Step 2: Update install instructions**

Replace `cargo install logmon-mcp-server` with:

```markdown
## Install

```
cd ~/Documents/Projects/MCPs/logmon-mcp
cargo install --path crates/broker --path crates/mcp
```

This places `logmon-broker` and `logmon-mcp` in `~/.cargo/bin/`.

### Run the broker as a system service (recommended)

```
logmon-broker install-service --scope user
```

Boots the broker via launchd (macOS) or systemd (Linux). Starts at login, restarts on crash.

### Register the MCP shim with Claude Code

```
claude mcp add logmon -- logmon-mcp
```

The shim auto-starts the broker if it isn't already running.

### Environment variable overrides

- `LOGMON_BROKER_BIN` — explicit path to `logmon-broker` (defeats PATH lookup).
- `LOGMON_BROKER_SOCKET` — explicit broker socket path (used by tests; defaults to `~/.config/logmon/logmon.sock`).
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs(README): workspace + broker architecture; new install commands"
```

---

### Task 27: Update logmon-mcp `skill/logmon.md`

**Files:**
- Modify: `/Users/yuval/Documents/Projects/MCPs/logmon-mcp/skill/logmon.md`

- [ ] **Step 1: Refresh architecture summary block**

Replace the existing "single-binary, two-mode" description with:

```markdown
## Architecture

logmon-broker is a long-lived daemon ingesting GELF (UDP/TCP) + OTLP (gRPC/HTTP). Multiple clients connect over a Unix domain socket via JSON-RPC 2.0:

- `logmon-mcp` — MCP shim (this skill's surface).
- `logmon-broker-sdk` — typed Rust client SDK for in-process consumers (e.g., `store-test`).
- Cross-language clients can codegen from `protocol-v1.schema.json`.

The daemon usually runs as a system service (`logmon-broker install-service`); the shim auto-starts it on demand if no service is installed.
```

- [ ] **Step 2: Add a paragraph at the bottom — Broker SDK forward link**

```markdown
## SDK consumers (forward link)

For non-MCP clients (test harnesses, archival workers, dashboards), see the typed Rust SDK at `crates/sdk` (`logmon-broker-sdk`). The first SDK consumer outside this repo is `store-test` — see Spec B for that integration.
```

- [ ] **Step 3: Commit**

```bash
git add skill/logmon.md
git commit -m "docs(skill): refresh architecture summary; note broker SDK + Spec B forward link"
```

---

### Task 28: Update Store project docs

**Files:**
- Modify: `/Users/yuval/Documents/Projects/Store/docs/guides/logmon.md` (binary names, deployment options paragraph)
- Modify: `/Users/yuval/Documents/Projects/Store/CLAUDE.md` (Resource Catalog row touchup if needed)
- Modify: `/Users/yuval/Documents/Projects/Store/.claude/CLAUDE.md` (binary name updates if any)
- Modify: `/Users/yuval/Documents/Projects/Store/.claude/skills/using-store-test/SKILL.md` (binary references if any)

- [ ] **Step 1: Audit references**

```
cd ~/Documents/Projects/Store
grep -RIn "logmon-mcp-server\|logmon-mcp\b" docs/ CLAUDE.md .claude/
```

For each hit, decide: if it's about the MCP shim (which is still called `logmon-mcp`), leave. If it's about the daemon binary (was `logmon-mcp-server`, now `logmon-broker`), update.

- [ ] **Step 2: Update `docs/guides/logmon.md`**

Add a deployment-options paragraph after the existing intro:

```markdown
## Deployment

The logmon broker daemon (`logmon-broker`) runs always-on as a launchd / systemd service. Install once:

```
logmon-broker install-service --scope user
```

The MCP shim (`logmon-mcp`) auto-starts the broker if no service is installed, so day-to-day usage is unaffected — the service install just makes startup-on-boot reliable.
```

- [ ] **Step 3: Update `CLAUDE.md` Resource Catalog**

Find the OpenTelemetry / Tracing row. Currently:

```
| OpenTelemetry / Tracing | `docs/guides/logmon.md` | `store_server/src/`, `ht_server/src/` | `/logmon` |
```

Confirm wording still applies. If the row mentions a binary name that has changed, update. Likely no change needed.

- [ ] **Step 4: Update `.claude/CLAUDE.md` and `using-store-test` skill**

For each found binary reference, update `logmon-mcp-server` → `logmon-mcp` (shim) or `logmon-broker` (daemon) as contextually appropriate. Filter-string examples are unaffected (the filter language hasn't changed).

- [ ] **Step 5: Commit (Store repo)**

```bash
cd ~/Documents/Projects/Store
git add docs/guides/logmon.md CLAUDE.md .claude/
git commit -m "docs: update logmon binary names + add broker deployment options paragraph"
```

---

## Final verification

- [ ] **Step 1: Full workspace build + test**

```
cd ~/Documents/Projects/MCPs/logmon-mcp
cargo build --workspace
cargo test --workspace --features test-support
cargo xtask verify-schema
```

Expected: all pass.

- [ ] **Step 2: Smoke tests from Task 25 README checklist**

Run the manual smoke-test checklist; all expectations met.

- [ ] **Step 3: Tag**

```bash
git tag broker-ification-v1 -m "logmon broker-ification: workspace + 5 crates + 2 binaries + typed SDK + system service"
```

- [ ] **Step 4: Push**

```bash
git push origin master
git push origin --tags
```

---

## Self-review against spec

Spec sections → tasks:

| Spec section | Task(s) |
|---|---|
| Workspace and crate layout | 1, 2, 3, 4, 5 |
| Source-tree mapping | 1–5 |
| Method inventory frozen | 7 (typed structs); no behavior change |
| `triggers.add` `oneshot` | 10 |
| `session.start` `client_info` | 11 |
| `session.start` `capabilities` | 12 |
| Version policy | (codified in protocol crate via PROTOCOL_VERSION = 1; no implementation needed) |
| JSON Schema export | 8 |
| Schema-drift guard | 9 |
| Notification namespace | 14 (Notification non_exhaustive enum reserves namespace) |
| `logmon-broker` CLI | 4, 18 |
| Lifecycle | 19, 20, 21 |
| `logmon-mcp` broker discovery | 5 (Step 6), 22 |
| Auto-start coexists with service | 22 (passive — no code change beyond discovery) |
| launchd plist | 23 |
| systemd unit | 24 |
| install / uninstall / status | 18, 23, 24 |
| Log routing under service | (no change — file-appender path unchanged) |
| `--system` macOS limitation | (documented in spec; not implemented in v1) |
| SDK connect-and-handshake | 5 (Step 3), 11 |
| SDK method dispatch | 13 |
| Filter builder | 15 |
| Notifications enum | 14 |
| Errors enum | 5 (Step 2) |
| Handle introspection | 5 (Step 3) |
| Drop semantics | (passive — Drop on Arc<Inner>; no explicit code) |
| Reconnection | 16, 17 |
| Subscriber contract | 14 (Notification enum), 16 (channel close on giveup) |
| Backward compatibility | (preserved by construction — no path/protocol changes) |
| Migration | (documented in spec + Task 26 README) |
| Testing strategy | tests scattered across Tasks 10, 11, 12, 13, 14, 15, 16, 17 |
| Documentation updates | 26, 27, 28 |

No spec requirement is unaddressed. Plan is complete.

---

**Plan complete and saved to `docs/superpowers/plans/2026-04-30-broker-ification.md`. Two execution options:**

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

**Which approach?**
