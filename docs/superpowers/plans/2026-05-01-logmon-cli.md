# logmon-mcp CLI Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a CLI mode to the existing `logmon-mcp` binary so Claude (and any shell consumer) can invoke broker operations through `Bash`-tool subcommands instead of MCP tool calls. Default invocation preserves today's MCP stdio behavior; new subcommands route through the typed SDK.

**Architecture:** Single binary, dual mode. Top-level `clap::Subcommand` enum on the existing `Cli` struct. When `command` is `None`, run today's MCP stdio path unchanged. When `command` is `Some(_)`, connect via SDK with CLI-tuned retry settings (no retry, 5s timeout) and dispatch to a handler in `crates/mcp/src/cli/<group>.rs`. Output goes through shared `format` helpers (block / table / json / error). 25 MCP tools map to 25 CLI subcommands across 7 groups.

**Tech Stack:** Rust workspace. New runtime dep: `comfy-table` for table output. New dev-dep: `logmon-broker-core` with `test-support` feature for integration tests against an in-process daemon. Existing deps (`clap`, `serde_json`, `tokio`, `logmon-broker-sdk`, `logmon-broker-protocol`) carry over.

**Spec:** `docs/superpowers/specs/2026-05-01-logmon-cli-design.md`. Read it before starting.

**Branch target:** new feature branch off `main` (cursor work is merged). Pre-flight: `git checkout -b feat/logmon-cli main`.

**Test count baseline:** 297 (post-cursor-feature-v1). Each task should report the new total.

---

## File map

**Created:**

- `crates/mcp/src/cli/mod.rs` — top-level CLI dispatcher, re-exports.
- `crates/mcp/src/cli/connect.rs` — CLI-tuned `Broker::connect` wrapper + transport-error formatter.
- `crates/mcp/src/cli/format.rs` — shared output helpers (block, table, json, error, truncation).
- `crates/mcp/src/cli/logs.rs` — `logs` subcommand group (recent, context, export, clear).
- `crates/mcp/src/cli/bookmarks.rs` — `bookmarks` subcommand group (add, list, remove, clear).
- `crates/mcp/src/cli/triggers.rs` — `triggers` subcommand group (add, list, edit, remove).
- `crates/mcp/src/cli/filters.rs` — `filters` subcommand group (add, list, edit, remove).
- `crates/mcp/src/cli/traces.rs` — `traces` subcommand group (recent, get, summary, slow, logs).
- `crates/mcp/src/cli/spans.rs` — `spans` subcommand group (context).
- `crates/mcp/src/cli/sessions.rs` — `sessions` subcommand group (list, drop).
- `crates/mcp/src/cli/status.rs` — `status` subcommand.
- `crates/mcp/tests/cli_status.rs` — integration test for `status`.
- `crates/mcp/tests/cli_logs.rs` — integration tests for the `logs` group.
- `crates/mcp/tests/cli_bookmarks.rs` — integration tests for the `bookmarks` group.
- `crates/mcp/tests/cli_triggers.rs` — integration tests for the `triggers` group.
- `crates/mcp/tests/cli_filters.rs` — integration tests for the `filters` group.
- `crates/mcp/tests/cli_traces_spans.rs` — integration tests for the `traces` + `spans` groups.
- `crates/mcp/tests/cli_sessions.rs` — integration tests for the `sessions` group.
- `crates/mcp/tests/cli_common.rs` — shared test helpers (build a binary command, spawn a test daemon, point CLI at it).

**Modified:**

- `crates/mcp/src/main.rs` — add `Subcommand` enum, dispatch on `command.is_some()`, preserve today's MCP-stdio path when `None`.
- `crates/mcp/Cargo.toml` — add `comfy-table` runtime dep, add `logmon-broker-core = { path = "../core", features = ["test-support"] }` to `[dev-dependencies]`, add `assert_cmd` and `tempfile` to `[dev-dependencies]` for integration tests.
- `crates/mcp/README.md` (new) — full CLI subcommand reference.
- `skill/logmon.md` — short note about CLI alternative for subagents and MCP-disconnect fallback.
- `README.md` — top-level mention of CLI mode in install/usage.

---

## Task 1: Subcommand scaffolding + comfy-table dep + verify MCP stdio path unchanged

**Files:**
- Modify: `crates/mcp/Cargo.toml`
- Modify: `crates/mcp/src/main.rs`
- Create: `crates/mcp/src/cli/mod.rs`

This task wires up the dispatch skeleton. Adding the `Subcommand` enum to `Cli` and routing on `command.is_some()` should not change MCP stdio behavior at all — the only path that runs in the `None` branch is today's code, untouched.

- [ ] **Step 1: Add comfy-table runtime dep**

In `crates/mcp/Cargo.toml`, append to `[dependencies]`:

```toml
comfy-table = "7"
```

(Pin the major; latest 7.x as of writing is 7.1.x. Cargo will pick the newest 7.* compatible.)

- [ ] **Step 2: Add Subcommand enum + dispatch wiring to main.rs**

Replace the entire body of `crates/mcp/src/main.rs` with:

```rust
use clap::{Parser, Subcommand as ClapSubcommand};
use logmon_broker_sdk::Broker;
use rmcp::ServiceExt;

mod auto_start;
mod cli;
mod notifications;
mod server;

#[derive(Parser, Debug)]
#[command(name = "logmon-mcp", version, about = "logmon broker MCP shim and CLI tool")]
struct Cli {
    #[command(subcommand)]
    command: Option<Subcommand>,

    /// Named session to attach to. Default for MCP stdio mode is anonymous.
    /// Default for CLI mode is "cli".
    #[arg(long, global = true)]
    session: Option<String>,

    /// Emit machine-readable JSON instead of human-readable text. CLI mode only.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(ClapSubcommand, Debug)]
enum Subcommand {
    /// Query and clear log entries.
    Logs(cli::logs::LogsCmd),
    /// Manage bookmarks (named seq positions, also referenced as cursors via c>=).
    Bookmarks(cli::bookmarks::BookmarksCmd),
    /// Manage triggers (filter-driven notifications fired on log match).
    Triggers(cli::triggers::TriggersCmd),
    /// Manage buffer filters (per-session).
    Filters(cli::filters::FiltersCmd),
    /// Query traces and trace summaries.
    Traces(cli::traces::TracesCmd),
    /// Query span context.
    Spans(cli::spans::SpansCmd),
    /// List or drop sessions.
    Sessions(cli::sessions::SessionsCmd),
    /// Print broker status (uptime, receivers, store stats).
    Status,
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

    match cli.command {
        Some(cmd) => {
            // CLI mode — short-lived, fail-fast, route to subcommand handler.
            let exit_code = cli::dispatch(cmd, cli.session, cli.json).await;
            std::process::exit(exit_code);
        }
        None => {
            // MCP stdio mode — today's path, unchanged.
            run_mcp_stdio(cli.session).await
        }
    }
}

async fn run_mcp_stdio(session: Option<String>) -> anyhow::Result<()> {
    auto_start::ensure_broker_running().await?;

    let mut builder = Broker::connect();
    if let Some(name) = session {
        builder = builder.session_name(name);
    }
    let broker = builder.open().await?;

    let mcp_server = server::GelfMcpServer::new(broker.clone());
    let service = mcp_server.serve(rmcp::transport::stdio()).await?;

    notifications::spawn_notification_forwarder(
        broker.subscribe_notifications(),
        service.peer().clone(),
    );

    service.waiting().await?;
    Ok(())
}
```

- [ ] **Step 3: Create cli/mod.rs with the dispatcher and stub modules**

Create `crates/mcp/src/cli/mod.rs`:

```rust
//! CLI mode for `logmon-mcp`. Routed when `Cli::command` is `Some(_)`.
//!
//! Each subcommand group lives in its own module and exposes:
//! - A `clap::Args`-style enum (e.g. `LogsCmd`) used by `main.rs`.
//! - An `async fn dispatch(broker, cmd, json) -> i32` that runs the command
//!   and returns the desired process exit code.

pub mod bookmarks;
pub mod connect;
pub mod filters;
pub mod format;
pub mod logs;
pub mod sessions;
pub mod spans;
pub mod status;
pub mod traces;
pub mod triggers;

use crate::Subcommand;

/// Top-level CLI dispatch. Returns the process exit code.
pub async fn dispatch(cmd: Subcommand, session: Option<String>, json: bool) -> i32 {
    let session_name = session.unwrap_or_else(|| "cli".to_string());

    let broker = match connect::connect_cli(&session_name, &cmd).await {
        Ok(b) => b,
        Err(e) => {
            format::error(&e.to_string(), json);
            return 1;
        }
    };

    match cmd {
        Subcommand::Logs(c) => logs::dispatch(&broker, c, json).await,
        Subcommand::Bookmarks(c) => bookmarks::dispatch(&broker, c, json).await,
        Subcommand::Triggers(c) => triggers::dispatch(&broker, c, json).await,
        Subcommand::Filters(c) => filters::dispatch(&broker, c, json).await,
        Subcommand::Traces(c) => traces::dispatch(&broker, c, json).await,
        Subcommand::Spans(c) => spans::dispatch(&broker, c, json).await,
        Subcommand::Sessions(c) => sessions::dispatch(&broker, c, json).await,
        Subcommand::Status => status::dispatch(&broker, json).await,
    }
}
```

Then create stub files (each empty for now, filled in later tasks). For each of: `bookmarks.rs`, `connect.rs`, `filters.rs`, `format.rs`, `logs.rs`, `sessions.rs`, `spans.rs`, `status.rs`, `traces.rs`, `triggers.rs`, write a placeholder that compiles:

```rust
// crates/mcp/src/cli/<NAME>.rs
//! <NAME> subcommand group — filled in by Task N.
```

That won't compile because `mod.rs` references types like `LogsCmd` and `dispatch`. Add the smallest possible scaffolding so `cargo build` passes. Each stub file:

```rust
// crates/mcp/src/cli/bookmarks.rs (mirror this in every group module)
use clap::Args;
use logmon_broker_sdk::Broker;

#[derive(Args, Debug)]
pub struct BookmarksCmd {}

pub async fn dispatch(_broker: &Broker, _cmd: BookmarksCmd, _json: bool) -> i32 {
    eprintln!("bookmarks subcommand not yet implemented");
    1
}
```

Replicate this shape for `logs.rs` (`LogsCmd`), `triggers.rs` (`TriggersCmd`), `filters.rs` (`FiltersCmd`), `traces.rs` (`TracesCmd`), `spans.rs` (`SpansCmd`), `sessions.rs` (`SessionsCmd`).

For `status.rs`, no `Cmd` type — bare:

```rust
// crates/mcp/src/cli/status.rs
use logmon_broker_sdk::Broker;

pub async fn dispatch(_broker: &Broker, _json: bool) -> i32 {
    eprintln!("status not yet implemented");
    1
}
```

For `connect.rs`:

```rust
// crates/mcp/src/cli/connect.rs
use anyhow::Result;
use logmon_broker_sdk::Broker;
use std::time::Duration;

use crate::Subcommand;

/// Connect to the broker with CLI-tuned retry settings:
/// - no reconnect (one-shot semantics),
/// - 5-second call timeout (fail fast).
pub async fn connect_cli(session: &str, cmd: &Subcommand) -> Result<Broker> {
    let argv = subcommand_argv(cmd);
    let client_info = serde_json::json!({
        "name": "logmon-mcp",
        "version": env!("CARGO_PKG_VERSION"),
        "mode": "cli",
        "argv": argv,
    });

    Broker::connect()
        .session_name(session)
        .client_info(client_info)
        .reconnect_max_attempts(0)
        .call_timeout(Duration::from_secs(5))
        .open()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "broker not running\n\nStart the broker with one of:\n  logmon-broker install-service --scope user   (recommended; daemonized via launchd/systemd)\n  logmon-broker                                 (foreground, for debugging)\n\nUnderlying error: {e}"
            )
        })
}

/// Extract the subcommand path (group + verb) for client_info. No flags or values.
fn subcommand_argv(cmd: &Subcommand) -> Vec<&'static str> {
    match cmd {
        Subcommand::Logs(_) => vec!["logs"],
        Subcommand::Bookmarks(_) => vec!["bookmarks"],
        Subcommand::Triggers(_) => vec!["triggers"],
        Subcommand::Filters(_) => vec!["filters"],
        Subcommand::Traces(_) => vec!["traces"],
        Subcommand::Spans(_) => vec!["spans"],
        Subcommand::Sessions(_) => vec!["sessions"],
        Subcommand::Status => vec!["status"],
    }
}
```

Note: each subcommand module's `Cmd` enum will later expose its inner verb so `subcommand_argv` can return e.g. `["logs", "recent"]` instead of just `["logs"]`. That refinement happens in each per-group task. For now, group-only is fine — the `client_info` payload is small either way.

For `format.rs`, write a minimal stub:

```rust
// crates/mcp/src/cli/format.rs
//! Shared output formatters for CLI mode.

/// Print an error in the appropriate format and return.
pub fn error(message: &str, json: bool) {
    if json {
        println!("{{\"error\":{}}}", serde_json::Value::String(message.to_string()));
    } else {
        eprintln!("error: {message}");
    }
}
```

- [ ] **Step 4: Build, verify nothing broke**

```bash
cd /Users/yuval/Documents/Projects/MCPs/logmon-mcp
cargo build --workspace --all-targets
```

Expected: clean compile, no warnings introduced. The new `comfy-table` dep is present but unused (allowed; we'll use it in Task 6).

- [ ] **Step 5: Verify MCP stdio mode still works**

```bash
echo '' | timeout 2 cargo run -p logmon-mcp 2>&1 | head -5 || true
```

Expected: process tries to start the MCP stdio server, possibly errors due to broker not running or stdio mismatch (but the dispatch path goes through `run_mcp_stdio` — today's behavior). The point is to verify `cli.command.is_none()` correctly routes to MCP mode.

A more targeted verification: run `cargo run -p logmon-mcp -- --help` and confirm both subcommands and global flags are listed:

```bash
cargo run -p logmon-mcp -- --help
```

Expected stdout includes:
- `Commands: logs, bookmarks, triggers, filters, traces, spans, sessions, status, help`
- `--session <SESSION>`
- `--json`

- [ ] **Step 6: Run all tests**

```bash
cargo test --workspace
```

Expected: 297 still passing. No new tests yet.

- [ ] **Step 7: Commit**

```bash
git add crates/mcp/Cargo.toml crates/mcp/src/main.rs crates/mcp/src/cli/
git commit -m "feat(mcp): subcommand scaffolding for CLI mode (no behavior change yet)"
```

---

## Task 2: Format helpers (block, table, json, error, truncation)

**Files:**
- Modify: `crates/mcp/src/cli/format.rs`

The format module is the shared output layer for every subcommand. It emits human-readable blocks (for log entries), aligned tables (for list-shaped results), JSON (with `--json`), and errors (with appropriate stream + format). Truncation policy is here.

- [ ] **Step 1: Write the failing tests**

At the bottom of `crates/mcp/src/cli/format.rs`, append:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_emit_pretty_serializes_value() {
        let v = json!({"a": 1, "b": [2, 3]});
        let out = json_string(&v);
        // Pretty-printed output spans multiple lines.
        assert!(out.contains('\n'));
        assert!(out.contains("\"a\": 1"));
    }

    #[test]
    fn truncate_blocks_under_limit_unchanged() {
        let blocks = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = truncate_blocks(blocks.clone(), 100, 100);
        assert_eq!(out, "a\n\nb\n\nc");
    }

    #[test]
    fn truncate_blocks_over_record_limit_appends_more_marker() {
        let blocks = (0..10).map(|i| format!("rec-{i}")).collect();
        let out = truncate_blocks(blocks, 5, usize::MAX);
        // First 5 records present, last 5 hidden behind marker.
        assert!(out.contains("rec-0"));
        assert!(out.contains("rec-4"));
        assert!(!out.contains("rec-5"));
        assert!(out.contains("... 5 more"));
    }

    #[test]
    fn truncate_blocks_over_byte_limit_truncates_records() {
        // 20 records of ~10 chars each = ~200 bytes; cap at 50 bytes.
        let blocks: Vec<String> = (0..20).map(|i| format!("record_{i:02}")).collect();
        let out = truncate_blocks(blocks, usize::MAX, 50);
        assert!(out.contains("record_00"));
        assert!(out.contains("more"));
        assert!(out.len() < 200);
    }

    #[test]
    fn error_human_writes_to_stderr_format() {
        // We can't easily capture stderr in a unit test; just ensure it doesn't panic.
        error("test error", false);
    }
}
```

- [ ] **Step 2: Run, expect failure**

```bash
cargo test -p logmon-mcp cli::format::tests
```

Expected: FAIL — `json_string` and `truncate_blocks` don't exist.

- [ ] **Step 3: Implement format helpers**

Replace the body of `crates/mcp/src/cli/format.rs` with:

```rust
//! Shared output formatters for CLI mode.

use serde::Serialize;

/// Default truncation thresholds for block-formatted output. Tuned to stay
/// well under typical Claude `Bash` output budgets.
pub const DEFAULT_MAX_BLOCK_RECORDS: usize = 50;
pub const DEFAULT_MAX_BLOCK_BYTES: usize = 16 * 1024;

/// Pretty-print a serializable value as JSON. Trailing newline included.
pub fn json_string<T: Serialize>(value: &T) -> String {
    let mut s = serde_json::to_string_pretty(value).unwrap_or_else(|e| {
        format!("{{\"error\":\"failed to serialize result: {e}\"}}")
    });
    s.push('\n');
    s
}

/// Print pretty JSON to stdout.
pub fn print_json<T: Serialize>(value: &T) {
    print!("{}", json_string(value));
}

/// Combine pre-formatted record blocks into a single human-readable string,
/// applying record-count and byte-count limits with a "... N more" marker
/// when truncation occurs.
pub fn truncate_blocks(
    blocks: Vec<String>,
    max_records: usize,
    max_bytes: usize,
) -> String {
    let total = blocks.len();
    let mut out = String::new();
    let mut emitted_records = 0usize;

    for (i, block) in blocks.iter().enumerate() {
        if emitted_records >= max_records {
            break;
        }
        // Check byte limit before adding the next block + separator.
        let separator_len = if i > 0 { 2 } else { 0 }; // "\n\n"
        if !out.is_empty() && out.len() + separator_len + block.len() > max_bytes {
            break;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(block);
        emitted_records += 1;
    }

    let remaining = total.saturating_sub(emitted_records);
    if remaining > 0 {
        out.push_str(&format!(
            "\n\n... {remaining} more record{plural}, use --json or refine the filter to see them",
            plural = if remaining == 1 { "" } else { "s" },
        ));
    }
    out
}

/// Print a list of pre-formatted blocks to stdout with default truncation.
pub fn print_blocks(blocks: Vec<String>) {
    let out = truncate_blocks(blocks, DEFAULT_MAX_BLOCK_RECORDS, DEFAULT_MAX_BLOCK_BYTES);
    println!("{out}");
}

/// Build a comfy-table from headers and rows. Caller passes pre-stringified cells.
pub fn build_table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    use comfy_table::{ContentArrangement, Table};

    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(headers.iter().copied());

    for row in rows {
        table.add_row(row);
    }

    table.to_string()
}

/// Print a comfy-table to stdout.
pub fn print_table(headers: &[&str], rows: Vec<Vec<String>>) {
    println!("{}", build_table(headers, rows));
}

/// Print an error message in the appropriate format. In `--json` mode emits
/// `{"error":"..."}` to stdout (so jq pipelines see structured output);
/// otherwise emits `error: ...` to stderr (UNIX convention for human users).
pub fn error(message: &str, json: bool) {
    if json {
        let v = serde_json::json!({ "error": message });
        print_json(&v);
    } else {
        eprintln!("error: {message}");
    }
}

// ---- existing #[cfg(test)] mod tests block from Step 1 stays here ----
```

- [ ] **Step 4: Run tests, expect pass**

```bash
cargo test -p logmon-mcp cli::format::tests
```

Expected: all 5 tests PASS.

- [ ] **Step 5: Run full workspace**

```bash
cargo test --workspace
```

Expected: 297 + 5 = 302 passing.

- [ ] **Step 6: Commit**

```bash
git add crates/mcp/src/cli/format.rs
git commit -m "feat(mcp/cli): format helpers (block, table, json, error, truncation)"
```

---

## Task 3: Test harness for integration tests

**Files:**
- Modify: `crates/mcp/Cargo.toml`
- Create: `crates/mcp/tests/cli_common.rs`

Integration tests for the CLI binary need three things:
1. Build the binary (cargo gives us `env!("CARGO_BIN_EXE_logmon-mcp")` automatically).
2. Spin up an in-process test daemon.
3. Tell the CLI to connect to the test daemon's socket, NOT `~/.config/logmon/logmon.sock`.

We need to add an env-var override path. Today's SDK reads `LOGMON_BROKER_SOCKET` (verify before relying on it). The test harness sets that env var to the test daemon's tempdir socket, then runs the CLI binary as a subprocess.

- [ ] **Step 1: Add dev-dependencies**

In `crates/mcp/Cargo.toml`, add a `[dev-dependencies]` section:

```toml
[dev-dependencies]
logmon-broker-core = { path = "../core", features = ["test-support"] }
assert_cmd = "2"
tempfile = { workspace = true }
predicates = "3"
```

- [ ] **Step 2: Verify the SDK already honors `LOGMON_BROKER_SOCKET`**

```bash
grep -n "LOGMON_BROKER_SOCKET\|env::var" crates/sdk/src/connect.rs
```

If the SDK's `default_socket_path` already checks `LOGMON_BROKER_SOCKET` first, we're good. (Per `docs/superpowers/specs/2026-04-30-broker-ification-design.md` and the broker-ification implementation, this is already the case.) If for any reason it isn't, you'll need to add the env-var read to `crates/sdk/src/connect.rs::default_socket_path()` — single-line `std::env::var("LOGMON_BROKER_SOCKET").ok().map(PathBuf::from).unwrap_or_else(...)`. Almost certainly already there.

- [ ] **Step 3: Author the shared test harness**

Create `crates/mcp/tests/cli_common.rs`:

```rust
//! Shared helpers for CLI integration tests.
//!
//! Each test:
//!   1. Spawns an in-process test daemon (via logmon_broker_core::test_support).
//!   2. Builds an `assert_cmd::Command` for the `logmon-mcp` binary with
//!      `LOGMON_BROKER_SOCKET` pointing at the daemon's socket.
//!   3. Asserts on stdout/stderr/exit.

#![cfg(feature = "test-support")]
#![allow(dead_code)] // shared helpers — used selectively per test file

use assert_cmd::Command;
use logmon_broker_core::test_support::{spawn_test_daemon, TestDaemonHandle};
use std::path::PathBuf;

/// Spawn a test daemon and return both the handle and a builder for `Command`
/// pointing at the daemon's socket.
pub async fn spawn_with_cli() -> (TestDaemonHandle, CliBuilder) {
    let daemon = spawn_test_daemon().await;
    let socket = daemon.socket_path.clone();
    (daemon, CliBuilder { socket })
}

pub struct CliBuilder {
    socket: PathBuf,
}

impl CliBuilder {
    /// Build an `assert_cmd::Command` for the logmon-mcp binary with
    /// `LOGMON_BROKER_SOCKET` set so the SDK connects to the test daemon.
    pub fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("logmon-mcp").expect("binary not built");
        cmd.env("LOGMON_BROKER_SOCKET", &self.socket);
        cmd
    }
}
```

- [ ] **Step 4: Build (the shared helper isn't a test target by itself — verify the dev-deps resolve)**

```bash
cargo build --workspace --all-targets
```

Expected: clean. No tests run (cli_common.rs has no `#[test]` or `#[tokio::test]`).

- [ ] **Step 5: Commit**

```bash
git add crates/mcp/Cargo.toml crates/mcp/tests/cli_common.rs
git commit -m "test(mcp/cli): shared test harness (assert_cmd + test_support daemon)"
```

---

## Task 4: `status` subcommand (smallest CLI command — gets the pattern right end-to-end)

**Files:**
- Modify: `crates/mcp/src/cli/status.rs`
- Create: `crates/mcp/tests/cli_status.rs`

`status` takes no args, no flags. Hits `broker.status_get(StatusGet {})`. Renders `daemon_uptime_secs`, `receivers`, and store stats (and the calling session's info). It's the simplest end-to-end exercise of the format helpers + connect helper.

- [ ] **Step 1: Write the failing integration test**

Create `crates/mcp/tests/cli_status.rs`:

```rust
mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn status_human_format_includes_uptime() {
    let (_daemon, cli) = spawn_with_cli().await;

    let assert = tokio::task::spawn_blocking(move || {
        cli.cmd().arg("status").assert()
    })
    .await
    .unwrap();

    assert.success().stdout(predicates::str::contains("uptime")).stdout(predicates::str::contains("receivers"));
}

#[tokio::test]
async fn status_json_emits_typed_struct() {
    let (_daemon, cli) = spawn_with_cli().await;

    let output = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["status", "--json"]).output().expect("subprocess failed")
    })
    .await
    .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json parse");
    assert!(v.get("daemon_uptime_secs").is_some(), "got: {v}");
    assert!(v.get("receivers").is_some(), "got: {v}");
}
```

- [ ] **Step 2: Run, expect failure**

```bash
cargo test -p logmon-mcp --test cli_status
```

Expected: FAIL. The stub `status::dispatch` exits with code 1 ("not yet implemented").

- [ ] **Step 3: Implement status::dispatch**

Replace `crates/mcp/src/cli/status.rs`:

```rust
//! `status` subcommand — print broker status (uptime, receivers, store stats, session).

use logmon_broker_protocol::StatusGet;
use logmon_broker_sdk::Broker;

use super::format;

pub async fn dispatch(broker: &Broker, json: bool) -> i32 {
    let result = match broker.status_get(StatusGet {}).await {
        Ok(r) => r,
        Err(e) => {
            format::error(&format!("status.get failed: {e}"), json);
            return 1;
        }
    };

    if json {
        format::print_json(&result);
        return 0;
    }

    // Human format: simple key=value block.
    println!("uptime: {}s", result.daemon_uptime_secs);
    print!("receivers:");
    if result.receivers.is_empty() {
        println!(" (none)");
    } else {
        println!();
        for r in &result.receivers {
            println!("  - {r}");
        }
    }
    println!(
        "store: total_received={} total_stored={} malformed={} current_size={}",
        result.store.total_received,
        result.store.total_stored,
        result.store.malformed_count,
        result.store.current_size,
    );
    if let Some(s) = &result.session {
        println!(
            "session: id={} name={} connected={} triggers={} filters={} queue={} last_seen={}s",
            s.id,
            s.name.as_deref().unwrap_or("(anonymous)"),
            s.connected,
            s.trigger_count,
            s.filter_count,
            s.queue_size,
            s.last_seen_secs_ago,
        );
    } else {
        println!("session: (none)");
    }
    0
}
```

- [ ] **Step 4: Run tests, expect pass**

```bash
cargo test -p logmon-mcp --test cli_status
```

Expected: both tests PASS.

- [ ] **Step 5: Run full workspace**

```bash
cargo test --workspace
```

Expected: 302 + 2 = 304 passing.

- [ ] **Step 6: Commit**

```bash
git add crates/mcp/src/cli/status.rs crates/mcp/tests/cli_status.rs
git commit -m "feat(mcp/cli): status subcommand"
```

---

## Task 5: `logs` subcommand group (recent, context, export, clear)

**Files:**
- Modify: `crates/mcp/src/cli/logs.rs`
- Create: `crates/mcp/tests/cli_logs.rs`

The `logs` group has 4 verbs: `recent`, `context`, `export`, `clear`. Each maps 1:1 to an SDK method. `recent` and `context` and `export` return log entries; render in block format. `clear` returns a `cleared` count; render as a one-line confirmation.

- [ ] **Step 1: Write the failing integration tests**

Create `crates/mcp/tests/cli_logs.rs`:

```rust
mod cli_common;

use cli_common::spawn_with_cli;
use logmon_broker_core::gelf::message::Level;

#[tokio::test]
async fn logs_recent_returns_injected_records() {
    let (daemon, cli) = spawn_with_cli().await;
    daemon.inject_log(Level::Info, "hello-cli").await;
    daemon.inject_log(Level::Error, "error-cli").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let output = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["logs", "recent", "--count", "10"]).output().unwrap()
    })
    .await
    .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello-cli"), "stdout: {stdout}");
    assert!(stdout.contains("error-cli"), "stdout: {stdout}");
}

#[tokio::test]
async fn logs_recent_json_returns_structured_logs() {
    let (daemon, cli) = spawn_with_cli().await;
    daemon.inject_log(Level::Info, "json-test").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let output = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["logs", "recent", "--count", "10", "--json"]).output().unwrap()
    })
    .await
    .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    let logs = v["logs"].as_array().expect("logs array");
    assert!(logs.iter().any(|l| l["message"] == "json-test"));
}

#[tokio::test]
async fn logs_recent_with_filter_passes_through() {
    let (daemon, cli) = spawn_with_cli().await;
    daemon.inject_log(Level::Info, "info-line").await;
    daemon.inject_log(Level::Error, "error-line").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let output = tokio::task::spawn_blocking(move || {
        cli.cmd()
            .args(["logs", "recent", "--filter", "l>=ERROR", "--count", "10"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("error-line"));
    assert!(!stdout.contains("info-line"), "expected only ERROR-level; got: {stdout}");
}

#[tokio::test]
async fn logs_clear_succeeds() {
    let (daemon, cli) = spawn_with_cli().await;
    daemon.inject_log(Level::Info, "to-clear").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let output = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["logs", "clear"]).output().unwrap()
    })
    .await
    .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cleared"), "stdout: {stdout}");
}

#[tokio::test]
async fn logs_recent_against_missing_broker_fails_with_guidance() {
    use assert_cmd::Command;

    let mut cmd = Command::cargo_bin("logmon-mcp").unwrap();
    cmd.env("LOGMON_BROKER_SOCKET", "/tmp/logmon-cli-test-nonexistent.sock");
    let output = tokio::task::spawn_blocking(move || {
        cmd.args(["logs", "recent"]).output().unwrap()
    })
    .await
    .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("broker not running") || stderr.contains("install-service"),
        "expected fail-fast guidance; got stderr: {stderr}"
    );
}
```

- [ ] **Step 2: Run, expect failure**

```bash
cargo test -p logmon-mcp --test cli_logs
```

Expected: FAIL. Stub returns 1 with "not yet implemented".

- [ ] **Step 3: Implement logs::dispatch**

Replace `crates/mcp/src/cli/logs.rs`:

```rust
//! `logs` subcommand group: recent, context, export, clear.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{
    LogEntry, LogsClear, LogsContext, LogsExport, LogsRecent,
};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct LogsCmd {
    #[command(subcommand)]
    verb: LogsVerb,
}

#[derive(Subcommand, Debug)]
enum LogsVerb {
    /// Fetch recent log entries (newest first by default; oldest first when filter contains c>=).
    Recent {
        #[arg(long, default_value_t = 50)]
        count: u64,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long, value_name = "TRACE_ID_HEX")]
        trace_id: Option<String>,
    },
    /// Fetch logs surrounding a specific seq.
    Context {
        #[arg(long)]
        seq: u64,
        #[arg(long, default_value_t = 10)]
        before: u64,
        #[arg(long, default_value_t = 10)]
        after: u64,
    },
    /// Export matching logs (returns the same shape as recent + a format field).
    Export {
        #[arg(long)]
        count: Option<u64>,
        #[arg(long)]
        filter: Option<String>,
        /// Write output to FILE instead of stdout. Use `-` for stdout. Existing files are overwritten.
        #[arg(long, value_name = "FILE")]
        out: Option<String>,
    },
    /// Clear the entire log buffer (affects all sessions).
    Clear,
}

pub async fn dispatch(broker: &Broker, cmd: LogsCmd, json: bool) -> i32 {
    match cmd.verb {
        LogsVerb::Recent { count, filter, trace_id } => {
            let params = LogsRecent {
                count: Some(count),
                filter,
                trace_id,
            };
            let result = match broker.logs_recent(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("logs.recent failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            let blocks: Vec<String> = result.logs.iter().map(format_entry).collect();
            format::print_blocks(blocks);
            if let Some(seq) = result.cursor_advanced_to {
                println!("\ncursor advanced to seq={seq}");
            }
            0
        }
        LogsVerb::Context { seq, before, after } => {
            let params = LogsContext {
                seq,
                before: Some(before),
                after: Some(after),
            };
            let result = match broker.logs_context(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("logs.context failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            let blocks: Vec<String> = result.logs.iter().map(format_entry).collect();
            format::print_blocks(blocks);
            0
        }
        LogsVerb::Export { count, filter, out } => {
            let params = LogsExport { count, filter };
            let result = match broker.logs_export(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("logs.export failed: {e}"), json); return 1; }
            };

            // Render the output payload first, then redirect to file or stdout.
            let payload = if json {
                format::json_string(&result)
            } else {
                let blocks: Vec<String> = result.logs.iter().map(format_entry).collect();
                let rendered = format::truncate_blocks(
                    blocks,
                    format::DEFAULT_MAX_BLOCK_RECORDS,
                    format::DEFAULT_MAX_BLOCK_BYTES,
                );
                format!("{rendered}\n")
            };

            match out.as_deref() {
                Some("-") | None => print!("{payload}"),
                Some(path) => {
                    if let Err(e) = std::fs::write(path, &payload) {
                        format::error(&format!("failed to write {path}: {e}"), json);
                        return 1;
                    }
                    if !json {
                        eprintln!("wrote {} log entries to {path}", result.count);
                    }
                }
            }
            0
        }
        LogsVerb::Clear => {
            let result = match broker.logs_clear(LogsClear {}).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("logs.clear failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("cleared {} log entries", result.cleared);
            0
        }
    }
}

/// Render a single log entry as a two-line block.
fn format_entry(e: &LogEntry) -> String {
    let header = format!(
        "[seq={}] {} {} {}: {}",
        e.seq,
        e.timestamp.to_rfc3339(),
        format_level(&e.level),
        e.facility.as_deref().unwrap_or("-"),
        e.message,
    );
    let mut secondary = Vec::new();
    if let Some(file) = &e.file {
        if let Some(line) = e.line {
            secondary.push(format!("file={file}:{line}"));
        } else {
            secondary.push(format!("file={file}"));
        }
    }
    if !e.host.is_empty() {
        secondary.push(format!("host={}", e.host));
    }
    if let Some(tid) = &e.trace_id {
        secondary.push(format!("trace={tid}"));
    }
    if let Some(sid) = &e.span_id {
        secondary.push(format!("span={sid}"));
    }
    if secondary.is_empty() {
        header
    } else {
        format!("{header}\n  {}", secondary.join(" "))
    }
}

fn format_level(l: &logmon_broker_protocol::Level) -> &'static str {
    use logmon_broker_protocol::Level::*;
    match l { Trace => "TRACE", Debug => "DEBUG", Info => "INFO", Warn => "WARN", Error => "ERROR" }
}
```

- [ ] **Step 4: Run tests, expect pass**

```bash
cargo test -p logmon-mcp --test cli_logs
```

Expected: 5 tests PASS.

- [ ] **Step 5: Run full workspace**

```bash
cargo test --workspace
```

Expected: 304 + 5 = 309 passing.

- [ ] **Step 6: Commit**

```bash
git add crates/mcp/src/cli/logs.rs crates/mcp/tests/cli_logs.rs
git commit -m "feat(mcp/cli): logs subcommand group (recent, context, export, clear)"
```

---

## Task 6: `bookmarks` subcommand group (add, list, remove, clear)

**Files:**
- Modify: `crates/mcp/src/cli/bookmarks.rs`
- Create: `crates/mcp/tests/cli_bookmarks.rs`

`bookmarks add` is the highest-traffic management command. List uses comfy-table. Remove/clear emit one-line confirmations.

- [ ] **Step 1: Write the failing integration tests**

Create `crates/mcp/tests/cli_bookmarks.rs`:

```rust
mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn bookmarks_add_then_list_round_trip() {
    let (_daemon, cli) = spawn_with_cli().await;

    let add = tokio::task::spawn_blocking({
        let cli = cli.cmd();
        move || {
            let mut c = cli;
            c.args(["bookmarks", "add", "--name", "my-anchor", "--start-seq", "42"]).output().unwrap()
        }
    }).await.unwrap();
    assert!(add.status.success(), "add failed: {}", String::from_utf8_lossy(&add.stderr));
    let add_stdout = String::from_utf8_lossy(&add.stdout);
    assert!(add_stdout.contains("my-anchor") || add_stdout.contains("seq=42"), "got: {add_stdout}");

    let list = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["bookmarks", "list", "--json"]).output().unwrap()
    }).await.unwrap();
    assert!(list.status.success());
    let v: serde_json::Value = serde_json::from_slice(&list.stdout).expect("json");
    let entries = v["bookmarks"].as_array().expect("bookmarks array");
    assert!(entries.iter().any(|b| b["seq"] == 42), "expected anchor with seq=42; got: {v}");
}

#[tokio::test]
async fn bookmarks_remove_succeeds() {
    let (_daemon, cli) = spawn_with_cli().await;

    let _ = tokio::task::spawn_blocking({
        let cli = cli.cmd();
        move || {
            let mut c = cli;
            c.args(["bookmarks", "add", "--name", "to-rm"]).output().unwrap()
        }
    }).await.unwrap();

    let rm = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["bookmarks", "remove", "--name", "cli/to-rm"]).output().unwrap()
    }).await.unwrap();
    assert!(rm.status.success(), "remove failed: {}", String::from_utf8_lossy(&rm.stderr));
}

#[tokio::test]
async fn bookmarks_add_replace_overwrites() {
    let (_daemon, cli) = spawn_with_cli().await;

    let _ = tokio::task::spawn_blocking({
        let cli = cli.cmd();
        move || {
            let mut c = cli;
            c.args(["bookmarks", "add", "--name", "x", "--start-seq", "1"]).output().unwrap()
        }
    }).await.unwrap();

    // Without --replace, second add errors.
    let dup = tokio::task::spawn_blocking({
        let cli = cli.cmd();
        move || {
            let mut c = cli;
            c.args(["bookmarks", "add", "--name", "x", "--start-seq", "2"]).output().unwrap()
        }
    }).await.unwrap();
    assert!(!dup.status.success());

    // With --replace, succeeds.
    let ok = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["bookmarks", "add", "--name", "x", "--start-seq", "2", "--replace"]).output().unwrap()
    }).await.unwrap();
    assert!(ok.status.success(), "replace failed: {}", String::from_utf8_lossy(&ok.stderr));
}
```

- [ ] **Step 2: Run, expect failure**

```bash
cargo test -p logmon-mcp --test cli_bookmarks
```

Expected: FAIL.

- [ ] **Step 3: Implement bookmarks::dispatch**

Replace `crates/mcp/src/cli/bookmarks.rs`:

```rust
//! `bookmarks` subcommand group: add, list, remove, clear.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{BookmarksAdd, BookmarksClear, BookmarksList, BookmarksRemove};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct BookmarksCmd {
    #[command(subcommand)]
    verb: BkmVerb,
}

#[derive(Subcommand, Debug)]
enum BkmVerb {
    /// Add a bookmark. Default start_seq is the current seq counter; pass --start-seq to override.
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        start_seq: Option<u64>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        replace: bool,
    },
    /// List all bookmarks (optionally filter by session).
    List {
        #[arg(long)]
        session: Option<String>,
    },
    /// Remove a bookmark by qualified name (`session/name`).
    Remove {
        #[arg(long)]
        name: String,
    },
    /// Clear bookmarks for a session (default = current session).
    Clear {
        #[arg(long)]
        session: Option<String>,
    },
}

pub async fn dispatch(broker: &Broker, cmd: BookmarksCmd, json: bool) -> i32 {
    match cmd.verb {
        BkmVerb::Add { name, start_seq, description, replace } => {
            let params = BookmarksAdd { name, description, start_seq, replace };
            let result = match broker.bookmarks_add(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("bookmarks.add failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            let action = if result.replaced { "replaced" } else { "added" };
            println!("bookmark {action}: {} (seq={})", result.qualified_name, result.seq);
            0
        }
        BkmVerb::List { session } => {
            let params = BookmarksList { session };
            let result = match broker.bookmarks_list(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("bookmarks.list failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            if result.bookmarks.is_empty() {
                println!("(no bookmarks)");
                return 0;
            }
            let rows: Vec<Vec<String>> = result.bookmarks.iter().map(|b| {
                vec![
                    b.qualified_name.clone(),
                    b.seq.to_string(),
                    b.created_at.to_rfc3339(),
                    b.description.clone().unwrap_or_default(),
                ]
            }).collect();
            format::print_table(&["name", "seq", "created_at", "description"], rows);
            0
        }
        BkmVerb::Remove { name } => {
            let params = BookmarksRemove { name };
            let result = match broker.bookmarks_remove(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("bookmarks.remove failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("removed: {}", result.removed);
            0
        }
        BkmVerb::Clear { session } => {
            let params = BookmarksClear { session };
            let result = match broker.bookmarks_clear(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("bookmarks.clear failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("cleared {} bookmark(s) for session {}", result.removed_count, result.session);
            0
        }
    }
}
```

- [ ] **Step 4: Run tests, expect pass**

```bash
cargo test -p logmon-mcp --test cli_bookmarks
```

Expected: 3 tests PASS.

- [ ] **Step 5: Run full workspace**

```bash
cargo test --workspace
```

Expected: 309 + 3 = 312 passing.

- [ ] **Step 6: Commit**

```bash
git add crates/mcp/src/cli/bookmarks.rs crates/mcp/tests/cli_bookmarks.rs
git commit -m "feat(mcp/cli): bookmarks subcommand group (add, list, remove, clear)"
```

---

## Task 7: `triggers` subcommand group (add, list, edit, remove)

**Files:**
- Modify: `crates/mcp/src/cli/triggers.rs`
- Create: `crates/mcp/tests/cli_triggers.rs`

Mirrors the bookmarks shape. Add `--oneshot` flag. List uses comfy-table.

- [ ] **Step 1: Write the failing tests**

Create `crates/mcp/tests/cli_triggers.rs`:

```rust
mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn triggers_add_then_list() {
    let (_daemon, cli) = spawn_with_cli().await;

    let add = tokio::task::spawn_blocking({
        let cli = cli.cmd();
        move || {
            let mut c = cli;
            c.args(["triggers", "add", "--filter", "l>=ERROR", "--oneshot", "--description", "test"]).output().unwrap()
        }
    }).await.unwrap();
    assert!(add.status.success(), "add stderr: {}", String::from_utf8_lossy(&add.stderr));

    let list = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["triggers", "list", "--json"]).output().unwrap()
    }).await.unwrap();
    assert!(list.status.success());
    let v: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let entries = v["triggers"].as_array().unwrap();
    assert!(
        entries.iter().any(|t| t["filter"] == "l>=ERROR" && t["oneshot"] == true),
        "expected oneshot ERROR trigger; got: {v}"
    );
}

#[tokio::test]
async fn triggers_remove_succeeds() {
    let (_daemon, cli) = spawn_with_cli().await;

    let add = tokio::task::spawn_blocking({
        let cli = cli.cmd();
        move || {
            let mut c = cli;
            c.args(["triggers", "add", "--filter", "l>=WARN", "--json"]).output().unwrap()
        }
    }).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&add.stdout).unwrap();
    let id = v["id"].as_u64().unwrap();

    let rm = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["triggers", "remove", "--id", &id.to_string()]).output().unwrap()
    }).await.unwrap();
    assert!(rm.status.success(), "rm stderr: {}", String::from_utf8_lossy(&rm.stderr));
}
```

- [ ] **Step 2: Run, expect failure**

```bash
cargo test -p logmon-mcp --test cli_triggers
```

Expected: FAIL.

- [ ] **Step 3: Implement triggers::dispatch**

Replace `crates/mcp/src/cli/triggers.rs`:

```rust
//! `triggers` subcommand group: add, list, edit, remove.
//!
//! Note: triggers added via CLI persist in the session but the CLI invocation
//! exits before any matching log can fire the trigger. Use CLI for trigger
//! management; subscribe to fires via the long-running MCP shim or a custom
//! SDK consumer.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{TriggersAdd, TriggersEdit, TriggersList, TriggersRemove};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct TriggersCmd {
    #[command(subcommand)]
    verb: TrgVerb,
}

#[derive(Subcommand, Debug)]
enum TrgVerb {
    /// Add a trigger. The CLI invocation exits before fires arrive — use the
    /// MCP shim to subscribe to firings.
    Add {
        #[arg(long)]
        filter: String,
        #[arg(long, default_value_t = 0)]
        pre_window: u32,
        #[arg(long, default_value_t = 0)]
        post_window: u32,
        #[arg(long, default_value_t = 0)]
        notify_context: u32,
        #[arg(long)]
        description: Option<String>,
        /// Auto-remove after the first fire.
        #[arg(long)]
        oneshot: bool,
    },
    List,
    Edit {
        #[arg(long)]
        id: u32,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        pre_window: Option<u32>,
        #[arg(long)]
        post_window: Option<u32>,
        #[arg(long)]
        notify_context: Option<u32>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        oneshot: Option<bool>,
    },
    Remove {
        #[arg(long)]
        id: u32,
    },
}

pub async fn dispatch(broker: &Broker, cmd: TriggersCmd, json: bool) -> i32 {
    match cmd.verb {
        TrgVerb::Add { filter, pre_window, post_window, notify_context, description, oneshot } => {
            let params = TriggersAdd {
                filter,
                pre_window: Some(pre_window),
                post_window: Some(post_window),
                notify_context: Some(notify_context),
                description,
                oneshot,
            };
            let result = match broker.triggers_add(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("triggers.add failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("trigger added: id={} oneshot={oneshot}", result.id);
            0
        }
        TrgVerb::List => {
            let result = match broker.triggers_list(TriggersList {}).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("triggers.list failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            if result.triggers.is_empty() { println!("(no triggers)"); return 0; }
            let rows: Vec<Vec<String>> = result.triggers.iter().map(|t| vec![
                t.id.to_string(),
                t.filter.clone(),
                format!("{}/{}/{}", t.pre_window, t.post_window, t.notify_context),
                t.match_count.to_string(),
                t.oneshot.to_string(),
                t.description.clone().unwrap_or_default(),
            ]).collect();
            format::print_table(&["id", "filter", "pre/post/notify", "fired", "oneshot", "description"], rows);
            0
        }
        TrgVerb::Edit { id, filter, pre_window, post_window, notify_context, description, oneshot } => {
            let params = TriggersEdit { id, filter, pre_window, post_window, notify_context, description, oneshot };
            let result = match broker.triggers_edit(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("triggers.edit failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("trigger edited: id={}", result.id);
            0
        }
        TrgVerb::Remove { id } => {
            let result = match broker.triggers_remove(TriggersRemove { id }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("triggers.remove failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("removed: id={}", result.removed);
            0
        }
    }
}
```

- [ ] **Step 4: Run tests, expect pass**

```bash
cargo test -p logmon-mcp --test cli_triggers
```

Expected: 2 tests PASS.

- [ ] **Step 5: Full workspace**

```bash
cargo test --workspace
```

Expected: 312 + 2 = 314 passing.

- [ ] **Step 6: Commit**

```bash
git add crates/mcp/src/cli/triggers.rs crates/mcp/tests/cli_triggers.rs
git commit -m "feat(mcp/cli): triggers subcommand group (add, list, edit, remove)"
```

---

## Task 8: `filters` subcommand group (add, list, edit, remove)

**Files:**
- Modify: `crates/mcp/src/cli/filters.rs`
- Create: `crates/mcp/tests/cli_filters.rs`

Same shape as triggers, simpler payload.

- [ ] **Step 1: Write the failing tests**

Create `crates/mcp/tests/cli_filters.rs`:

```rust
mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn filters_add_list_remove() {
    let (_daemon, cli) = spawn_with_cli().await;

    let add = tokio::task::spawn_blocking({
        let cli = cli.cmd();
        move || {
            let mut c = cli;
            c.args(["filters", "add", "--filter", "fa=mqtt", "--description", "mqtt-only", "--json"]).output().unwrap()
        }
    }).await.unwrap();
    assert!(add.status.success(), "add stderr: {}", String::from_utf8_lossy(&add.stderr));
    let v: serde_json::Value = serde_json::from_slice(&add.stdout).unwrap();
    let id = v["id"].as_u64().unwrap();

    let list = tokio::task::spawn_blocking({
        let cli = cli.cmd();
        move || {
            let mut c = cli;
            c.args(["filters", "list", "--json"]).output().unwrap()
        }
    }).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert!(v["filters"].as_array().unwrap().iter().any(|f| f["filter"] == "fa=mqtt"));

    let rm = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["filters", "remove", "--id", &id.to_string()]).output().unwrap()
    }).await.unwrap();
    assert!(rm.status.success());
}
```

- [ ] **Step 2: Run, expect failure**

```bash
cargo test -p logmon-mcp --test cli_filters
```

Expected: FAIL.

- [ ] **Step 3: Implement filters::dispatch**

Replace `crates/mcp/src/cli/filters.rs`:

```rust
//! `filters` subcommand group: add, list, edit, remove.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{FiltersAdd, FiltersEdit, FiltersList, FiltersRemove};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct FiltersCmd {
    #[command(subcommand)]
    verb: FltVerb,
}

#[derive(Subcommand, Debug)]
enum FltVerb {
    Add {
        #[arg(long)]
        filter: String,
        #[arg(long)]
        description: Option<String>,
    },
    List,
    Edit {
        #[arg(long)]
        id: u32,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    Remove {
        #[arg(long)]
        id: u32,
    },
}

pub async fn dispatch(broker: &Broker, cmd: FiltersCmd, json: bool) -> i32 {
    match cmd.verb {
        FltVerb::Add { filter, description } => {
            let result = match broker.filters_add(FiltersAdd { filter, description }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("filters.add failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("filter added: id={}", result.id);
            0
        }
        FltVerb::List => {
            let result = match broker.filters_list(FiltersList {}).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("filters.list failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            if result.filters.is_empty() { println!("(no filters)"); return 0; }
            let rows: Vec<Vec<String>> = result.filters.iter().map(|f| vec![
                f.id.to_string(),
                f.filter.clone(),
                f.description.clone().unwrap_or_default(),
            ]).collect();
            format::print_table(&["id", "filter", "description"], rows);
            0
        }
        FltVerb::Edit { id, filter, description } => {
            let result = match broker.filters_edit(FiltersEdit { id, filter, description }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("filters.edit failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("filter edited: id={}", result.id);
            0
        }
        FltVerb::Remove { id } => {
            let result = match broker.filters_remove(FiltersRemove { id }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("filters.remove failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("removed: id={}", result.removed);
            0
        }
    }
}
```

- [ ] **Step 4: Run tests, expect pass**

```bash
cargo test -p logmon-mcp --test cli_filters
```

Expected: 1 test PASS.

- [ ] **Step 5: Full workspace**

```bash
cargo test --workspace
```

Expected: 314 + 1 = 315 passing.

- [ ] **Step 6: Commit**

```bash
git add crates/mcp/src/cli/filters.rs crates/mcp/tests/cli_filters.rs
git commit -m "feat(mcp/cli): filters subcommand group (add, list, edit, remove)"
```

---

## Task 9: `traces` + `spans` subcommand groups

**Files:**
- Modify: `crates/mcp/src/cli/traces.rs`
- Modify: `crates/mcp/src/cli/spans.rs`
- Create: `crates/mcp/tests/cli_traces_spans.rs`

`traces` has 5 verbs (recent, get, summary, slow, logs); `spans` has 1 (context). Both are read-only; render trace summaries and spans as block format with secondary fields.

- [ ] **Step 1: Write the failing tests**

Create `crates/mcp/tests/cli_traces_spans.rs`:

```rust
mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn traces_recent_empty_returns_zero_count() {
    let (_daemon, cli) = spawn_with_cli().await;
    let output = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["traces", "recent", "--json"]).output().unwrap()
    }).await.unwrap();
    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["count"], 0);
}

#[tokio::test]
async fn traces_get_unknown_trace_returns_empty() {
    let (_daemon, cli) = spawn_with_cli().await;
    let output = tokio::task::spawn_blocking(move || {
        cli.cmd().args([
            "traces", "get",
            "--trace-id", "00000000000000000000000000000001",
            "--json",
        ]).output().unwrap()
    }).await.unwrap();
    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["span_count"], 0);
}

#[tokio::test]
async fn spans_context_empty_returns_zero() {
    let (_daemon, cli) = spawn_with_cli().await;
    let output = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["spans", "context", "--seq", "1", "--json"]).output().unwrap()
    }).await.unwrap();
    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["count"], 0);
}
```

- [ ] **Step 2: Run, expect failure**

```bash
cargo test -p logmon-mcp --test cli_traces_spans
```

Expected: FAIL.

- [ ] **Step 3: Implement traces::dispatch**

Replace `crates/mcp/src/cli/traces.rs`:

```rust
//! `traces` subcommand group: recent, get, summary, slow, logs.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{
    LogEntry, SpanEntry, TraceSummary, TracesGet, TracesLogs, TracesRecent,
    TracesSlow, TracesSummary,
};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct TracesCmd {
    #[command(subcommand)]
    verb: TrcVerb,
}

#[derive(Subcommand, Debug)]
enum TrcVerb {
    Recent {
        #[arg(long, default_value_t = 20)]
        count: u64,
        #[arg(long)]
        filter: Option<String>,
    },
    Get {
        #[arg(long)]
        trace_id: String,
        #[arg(long)]
        include_logs: bool,
        #[arg(long)]
        filter: Option<String>,
    },
    Summary {
        #[arg(long)]
        trace_id: String,
    },
    Slow {
        #[arg(long)]
        min_duration_ms: Option<f64>,
        #[arg(long)]
        count: Option<u64>,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        group_by: Option<String>,
    },
    Logs {
        #[arg(long)]
        trace_id: String,
        #[arg(long)]
        filter: Option<String>,
    },
}

pub async fn dispatch(broker: &Broker, cmd: TracesCmd, json: bool) -> i32 {
    match cmd.verb {
        TrcVerb::Recent { count, filter } => {
            let result = match broker.traces_recent(TracesRecent { count: Some(count), filter }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("traces.recent failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            if result.traces.is_empty() { println!("(no traces)"); return 0; }
            let rows: Vec<Vec<String>> = result.traces.iter().map(format_trace_row).collect();
            format::print_table(&["trace_id", "service", "root", "duration_ms", "spans", "errors"], rows);
            0
        }
        TrcVerb::Get { trace_id, include_logs, filter } => {
            let result = match broker.traces_get(TracesGet {
                trace_id, include_logs: Some(include_logs), filter,
            }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("traces.get failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("trace: {} ({} spans, {} logs)", result.trace_id, result.span_count, result.log_count);
            let blocks: Vec<String> = result.spans.iter().map(format_span).collect();
            format::print_blocks(blocks);
            0
        }
        TrcVerb::Summary { trace_id } => {
            let result = match broker.traces_summary(TracesSummary { trace_id }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("traces.summary failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("root: {}", result.root_span);
            println!("total: {:.1}ms ({} spans)", result.total_duration_ms, result.span_count);
            for entry in &result.breakdown {
                println!(
                    "  {:>5.1}%  {:.1}ms  self={:.1}ms  {}{}",
                    entry.percentage, entry.total_time_ms, entry.self_time_ms,
                    entry.name,
                    if entry.is_error { " (error)" } else { "" },
                );
            }
            if result.other_ms > 0.1 {
                println!("  ?       {:.1}ms  (other)", result.other_ms);
            }
            0
        }
        TrcVerb::Slow { min_duration_ms, count, filter, group_by } => {
            let result = match broker.traces_slow(TracesSlow { min_duration_ms, count, filter, group_by }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("traces.slow failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            if let Some(groups) = &result.groups {
                if groups.is_empty() { println!("(no slow spans)"); return 0; }
                let rows: Vec<Vec<String>> = groups.iter().map(|g| vec![
                    g.name.clone(),
                    format!("{}", g.count),
                    format!("{:.1}", g.avg_ms),
                    format!("{:.1}", g.p95_ms),
                ]).collect();
                format::print_table(&["name", "count", "avg_ms", "p95_ms"], rows);
            } else if let Some(spans) = &result.spans {
                if spans.is_empty() { println!("(no slow spans)"); return 0; }
                let blocks: Vec<String> = spans.iter().map(format_span).collect();
                format::print_blocks(blocks);
            } else {
                println!("(empty result)");
            }
            0
        }
        TrcVerb::Logs { trace_id, filter } => {
            let result = match broker.traces_logs(TracesLogs { trace_id, filter }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("traces.logs failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            let blocks: Vec<String> = result.logs.iter().map(format_log_oneline).collect();
            format::print_blocks(blocks);
            if let Some(seq) = result.cursor_advanced_to {
                println!("\ncursor advanced to seq={seq}");
            }
            0
        }
    }
}

fn format_trace_row(t: &TraceSummary) -> Vec<String> {
    vec![
        t.trace_id.clone(),
        t.service_name.clone(),
        t.root_span_name.clone(),
        format!("{:.1}", t.total_duration_ms),
        t.span_count.to_string(),
        if t.has_errors { "yes" } else { "no" }.to_string(),
    ]
}

fn format_span(s: &SpanEntry) -> String {
    let parent = s.parent_span_id.as_deref().unwrap_or("(root)");
    let status = match &s.status {
        logmon_broker_protocol::SpanStatus::Ok => "ok".to_string(),
        logmon_broker_protocol::SpanStatus::Unset => "unset".to_string(),
        logmon_broker_protocol::SpanStatus::Error(m) => format!("error: {m}"),
    };
    format!(
        "[seq={}] {} {} ({:.1}ms) status={status}\n  trace={} span={} parent={parent} service={}",
        s.seq, s.start_time.to_rfc3339(), s.name, s.duration_ms,
        s.trace_id, s.span_id, s.service_name,
    )
}

fn format_log_oneline(e: &LogEntry) -> String {
    format!(
        "[seq={}] {} {:?} {}",
        e.seq, e.timestamp.to_rfc3339(), e.level, e.message
    )
}
```

- [ ] **Step 4: Implement spans::dispatch**

Replace `crates/mcp/src/cli/spans.rs`:

```rust
//! `spans` subcommand group: context.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{SpanEntry, SpansContext};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct SpansCmd {
    #[command(subcommand)]
    verb: SpnVerb,
}

#[derive(Subcommand, Debug)]
enum SpnVerb {
    Context {
        #[arg(long)]
        seq: u64,
        #[arg(long, default_value_t = 5)]
        before: u64,
        #[arg(long, default_value_t = 5)]
        after: u64,
    },
}

pub async fn dispatch(broker: &Broker, cmd: SpansCmd, json: bool) -> i32 {
    match cmd.verb {
        SpnVerb::Context { seq, before, after } => {
            let params = SpansContext { seq, before: Some(before), after: Some(after) };
            let result = match broker.spans_context(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("spans.context failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            let blocks: Vec<String> = result.spans.iter().map(format_span).collect();
            format::print_blocks(blocks);
            0
        }
    }
}

fn format_span(s: &SpanEntry) -> String {
    let parent = s.parent_span_id.as_deref().unwrap_or("(root)");
    format!(
        "[seq={}] {} {} ({:.1}ms)\n  trace={} span={} parent={parent}",
        s.seq, s.start_time.to_rfc3339(), s.name, s.duration_ms,
        s.trace_id, s.span_id,
    )
}
```

- [ ] **Step 5: Run tests, expect pass**

```bash
cargo test -p logmon-mcp --test cli_traces_spans
```

Expected: 3 tests PASS.

- [ ] **Step 6: Full workspace**

```bash
cargo test --workspace
```

Expected: 315 + 3 = 318 passing.

- [ ] **Step 7: Commit**

```bash
git add crates/mcp/src/cli/traces.rs crates/mcp/src/cli/spans.rs crates/mcp/tests/cli_traces_spans.rs
git commit -m "feat(mcp/cli): traces + spans subcommand groups"
```

---

## Task 10: `sessions` subcommand group (list, drop)

**Files:**
- Modify: `crates/mcp/src/cli/sessions.rs`
- Create: `crates/mcp/tests/cli_sessions.rs`

Last subcommand group. Simple: `list` (table format) and `drop` (one-line confirmation).

- [ ] **Step 1: Write the failing tests**

Create `crates/mcp/tests/cli_sessions.rs`:

```rust
mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn sessions_list_includes_cli_session() {
    let (_daemon, cli) = spawn_with_cli().await;
    let output = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["sessions", "list", "--json"]).output().unwrap()
    }).await.unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let sessions = v["sessions"].as_array().unwrap();
    assert!(
        sessions.iter().any(|s| s["name"] == "cli"),
        "expected named session 'cli'; got: {v}"
    );
}
```

- [ ] **Step 2: Run, expect failure**

```bash
cargo test -p logmon-mcp --test cli_sessions
```

Expected: FAIL.

- [ ] **Step 3: Implement sessions::dispatch**

Replace `crates/mcp/src/cli/sessions.rs`:

```rust
//! `sessions` subcommand group: list, drop.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{SessionDrop, SessionList};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct SessionsCmd {
    #[command(subcommand)]
    verb: SesVerb,
}

#[derive(Subcommand, Debug)]
enum SesVerb {
    List,
    Drop {
        #[arg(long)]
        name: String,
    },
}

pub async fn dispatch(broker: &Broker, cmd: SessionsCmd, json: bool) -> i32 {
    match cmd.verb {
        SesVerb::List => {
            let result = match broker.session_list(SessionList {}).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("session.list failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            if result.sessions.is_empty() { println!("(no sessions)"); return 0; }
            let rows: Vec<Vec<String>> = result.sessions.iter().map(|s| vec![
                s.id.clone(),
                s.name.clone().unwrap_or_else(|| "(anonymous)".into()),
                s.connected.to_string(),
                s.trigger_count.to_string(),
                s.filter_count.to_string(),
                s.queue_size.to_string(),
                format!("{}s", s.last_seen_secs_ago),
            ]).collect();
            format::print_table(
                &["id", "name", "connected", "triggers", "filters", "queue", "last_seen"],
                rows,
            );
            0
        }
        SesVerb::Drop { name } => {
            let result = match broker.session_drop(SessionDrop { name }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("session.drop failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("dropped: {}", result.dropped);
            0
        }
    }
}
```

- [ ] **Step 4: Run tests, expect pass**

```bash
cargo test -p logmon-mcp --test cli_sessions
```

Expected: 1 test PASS.

- [ ] **Step 5: Full workspace**

```bash
cargo test --workspace
```

Expected: 318 + 1 = 319 passing.

- [ ] **Step 6: Commit**

```bash
git add crates/mcp/src/cli/sessions.rs crates/mcp/tests/cli_sessions.rs
git commit -m "feat(mcp/cli): sessions subcommand group (list, drop)"
```

---

## Task 11: Documentation — README + skill update

**Files:**
- Create: `crates/mcp/README.md`
- Modify: `skill/logmon.md`
- Modify: `README.md` (top-level)

Document the new CLI surface so Claude / users / store-test can discover it.

- [ ] **Step 1: Author crates/mcp/README.md**

Create `crates/mcp/README.md`:

```markdown
# logmon-mcp

The `logmon-mcp` binary serves two roles:

1. **MCP stdio server** — invoked without subcommands, runs as the MCP shim that Claude Code, Cursor, etc. connect to via `claude mcp add logmon -- logmon-mcp`. This is today's default behavior, unchanged.

2. **CLI tool** — invoked with subcommands, performs broker operations from the shell. Mirrors the MCP tool surface 1:1. Useful when MCP isn't available (subagents, cross-tool consumers, CI scripts), or when you want pipe-friendly output.

Both modes connect to the same broker daemon (`logmon-broker`) over the same Unix domain socket.

## CLI quick reference

```
logmon-mcp [--session NAME] [--json] <COMMAND>
```

Global flags:
- `--session NAME`: connect to a named session. Default for CLI mode is `"cli"` so state persists across invocations.
- `--json`: emit machine-readable JSON. Default is human-readable text.

### Commands

| Group | Verb | Description |
|---|---|---|
| `logs` | `recent` | Fetch recent logs (newest-first; oldest-first when filter contains `c>=`). |
| `logs` | `context` | Fetch logs surrounding a specific seq. |
| `logs` | `export` | Export matching logs (with `--out FILE` to redirect). |
| `logs` | `clear` | Clear the log buffer. |
| `bookmarks` | `add` | Add a bookmark (a named seq position). |
| `bookmarks` | `list` | List bookmarks. |
| `bookmarks` | `remove` | Remove a bookmark by qualified name. |
| `bookmarks` | `clear` | Clear all bookmarks for a session. |
| `triggers` | `add` | Add a trigger (notification fires require an MCP shim subscriber). |
| `triggers` | `list` / `edit` / `remove` | Manage triggers. |
| `filters` | `add` / `list` / `edit` / `remove` | Manage per-session buffer filters. |
| `traces` | `recent` / `get` / `summary` / `slow` / `logs` | Query traces. |
| `spans` | `context` | Fetch spans surrounding a seq. |
| `sessions` | `list` / `drop` | List or drop sessions. |
| `status` | (no verb) | Print broker status. |

Run `logmon-mcp <group> --help` for per-group flag details.

## Notes

- **Triggers don't fire in CLI mode.** A CLI invocation exits before any matching log can fire the trigger. Use the CLI to *manage* triggers; subscribe to fires via the MCP shim or a custom SDK consumer.
- **The CLI is one-shot.** No reconnect, 5-second call timeout. Errors fast if the broker isn't running.
- **No auto-start.** Install the broker as a service: `logmon-broker install-service --scope user`.
```

- [ ] **Step 2: Update skill/logmon.md**

Find the section explaining the broker architecture (probably near the top of the file). Inject after it:

```bash
grep -n "## Architecture\|## Setup\|^# logmon" /Users/yuval/Documents/Projects/MCPs/logmon-mcp/skill/logmon.md | head
```

Then edit the file to add a paragraph after the Architecture section:

```markdown
## CLI alternative (Bash tool)

In addition to the MCP tools above, the same operations are available as `logmon-mcp <subcommand>` invocations. This is useful when:

- You're in a subagent (subagents launched via the Agent tool don't inherit MCP servers).
- The MCP server has disconnected mid-session.
- You want to pipe output through `head`, `jq`, or `grep`.

The mapping is mechanical: `mcp__logmon__get_recent_logs` ⇔ `logmon-mcp logs recent`, `mcp__logmon__add_bookmark` ⇔ `logmon-mcp bookmarks add`, etc. See `crates/mcp/README.md` for the full reference.

CLI sessions default to a named session called `"cli"`, so bookmarks and other session state persist across CLI invocations. Use `--session NAME` to isolate.
```

- [ ] **Step 3: Update top-level README.md**

In the install/usage section (find it via `grep -n "## Install\|## Usage" /Users/yuval/Documents/Projects/MCPs/logmon-mcp/README.md`), add a sentence near the existing `cargo install` instructions:

```
After `cargo install`, the `logmon-mcp` binary is available in two modes: as the MCP stdio shim (today's behavior) and as a CLI for shell consumers (`logmon-mcp <subcommand>` — see crates/mcp/README.md).
```

- [ ] **Step 4: Verify docs are coherent**

```bash
ls /Users/yuval/Documents/Projects/MCPs/logmon-mcp/crates/mcp/README.md
grep -c "logmon-mcp" /Users/yuval/Documents/Projects/MCPs/logmon-mcp/skill/logmon.md
```

- [ ] **Step 5: Commit**

```bash
git add crates/mcp/README.md skill/logmon.md README.md
git commit -m "docs(mcp/cli): README + skill update + top-level mention of CLI mode"
```

---

## Task 12: Final regression sweep + verify MCP stdio path unchanged

**Files:**
- (Verification only — no code or doc changes)

End-to-end sanity check: workspace tests still pass, schema is clean, MCP stdio path is unchanged, all CLI subcommands respond to `--help`.

- [ ] **Step 1: Full workspace test sweep**

```bash
cd /Users/yuval/Documents/Projects/MCPs/logmon-mcp
cargo build --workspace --all-targets
cargo test --workspace
cargo xtask verify-schema
```

Expected: clean build (no warnings), 319 tests passing, schema up to date (no protocol changes in this feature).

- [ ] **Step 2: Spot-check `--help` output for all subcommand groups**

```bash
for group in logs bookmarks triggers filters traces spans sessions; do
    echo "=== $group ==="
    cargo run --quiet -p logmon-mcp -- $group --help | head -20
    echo
done
```

Expected: each group shows its verbs and flags. No panics, no errors.

- [ ] **Step 3: MCP stdio path smoke test**

Verify today's MCP stdio mode still works. With the broker running:

```bash
~/.cargo/bin/logmon-broker status
# (should print "running pid=N socket=...")

echo '' | timeout 1 cargo run --quiet -p logmon-mcp || true
```

The `timeout 1` exits after 1s (the MCP server is waiting for stdio input). The point: no crash, no clap parse error, the binary starts in MCP mode when no subcommand is given.

- [ ] **Step 4: Live broker smoke test (optional but recommended)**

Against your installed broker:

```bash
~/.cargo/bin/logmon-broker install-service --scope user 2>&1 | head -1   # idempotent
sleep 1
cargo run --quiet -p logmon-mcp -- status
cargo run --quiet -p logmon-mcp -- logs recent --count 5
cargo run --quiet -p logmon-mcp -- bookmarks list
cargo run --quiet -p logmon-mcp -- sessions list
```

Expected: `status` prints the running broker's info; `logs recent` returns whatever's in the live buffer (likely many entries from prior usage); `bookmarks list` may show prior `cli/*` bookmarks; `sessions list` shows the `cli` session and any other live sessions.

- [ ] **Step 5: Final commit (and tag)**

```bash
git tag logmon-cli-v1 -m "logmon-mcp CLI mode (1:1 mirror of MCP tool surface, 25 subcommands across 7 groups)"
git log --oneline main..HEAD | head -20
```

(No new commit at this step — tag points at the doc commit from Task 11. If you'd rather have an explicit "completion" commit, that's optional.)

---

## Self-review (after writing the plan)

**Spec coverage:** spec at `docs/superpowers/specs/2026-05-01-logmon-cli-design.md`. Walking through each section:

- §Architecture (single binary, dual mode, dispatch on `command.is_some()`) → Task 1.
- §Subcommand structure (7 groups, 25 verbs) → Tasks 4–10.
- §Output format (block, table, json, error, truncation) → Task 2.
- §Session model (default `"cli"`, `--session NAME` override) → Task 1 (dispatcher) + Task 3 (test harness uses `LOGMON_BROKER_SOCKET` so session naming is exercised).
- §Errors and exit codes (0/1/2; stderr human, stdout JSON) → Task 2 (`format::error`) + each subcommand's dispatch (returns `i32`).
- §Connection timeout / retry policy (no retry, 5s timeout) → Task 1 (`connect.rs`).
- §No auto-start → Task 1 (error message in `connect.rs`).
- §Implementation footprint → Tasks 1–10 (all listed files created).
- §Documentation → Task 11.
- §Out of scope (notifications, custom output templates, bulk-from-file, shell completions, color) → not implemented; nothing required.
- §Testing (per-group integration tests) → Tasks 4–10 (each task adds a test file).
- §Migration / back-compat → Task 1 (default `command = None` preserves MCP stdio behavior).

**Placeholder scan:** no "TBD/TODO/etc." All steps have actual code or commands.

**Type consistency:** `Subcommand` enum naming (`Logs`, `Bookmarks`, etc.) is consistent across `main.rs` and `cli/mod.rs`. Group structs (`LogsCmd`, `BookmarksCmd`, etc.) have matching names with their modules. The `dispatch(broker, cmd, json) -> i32` signature is uniform across all subcommand modules. `format::print_json`, `format::print_blocks`, `format::print_table`, `format::error` are consistently named. SDK method names (`broker.status_get(...)`, `broker.bookmarks_add(...)`, etc.) match what the SDK actually exposes (verified against the cursor-feature-v1 commit).

**Test count progression:** 297 → 297 (Task 1, no new tests; scaffolding only) → 302 (Task 2 +5 unit) → 302 (Task 3, harness only, no test) → 304 (Task 4 +2) → 309 (Task 5 +5) → 312 (Task 6 +3) → 314 (Task 7 +2) → 315 (Task 8 +1) → 318 (Task 9 +3) → 319 (Task 10 +1) → 319 (Task 11, docs) → 319 (Task 12, verification).

Final test count: 319.
