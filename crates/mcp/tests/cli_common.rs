//! Shared helpers for CLI integration tests.
//!
//! Each test:
//!   1. Spawns an in-process test daemon (via logmon_broker_core::test_support
//!      — available because the dev-dep enables that feature on the core crate).
//!   2. Builds an `assert_cmd::Command` for the `logmon-mcp` binary with
//!      `LOGMON_BROKER_SOCKET` pointing at the daemon's socket.
//!   3. Asserts on stdout/stderr/exit.
//!
//! Note: do NOT add `#![cfg(feature = "test-support")]` here — the logmon-mcp
//! crate has no `test-support` feature; the dev-dep enables that feature on
//! `logmon-broker-core` only. A cfg gate would always evaluate false and the
//! module would compile to nothing, breaking every test that imports it.
//!
//! The 50ms `tokio::sleep` after `inject_log` calls in tests is a known fragile
//! pattern (under load it can race the log_processor). It mirrors the cursor
//! test pattern; future hardening would wire a deterministic ingest barrier
//! into the harness.

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
