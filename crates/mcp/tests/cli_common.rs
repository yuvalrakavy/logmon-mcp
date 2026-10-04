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
    /// A builder pointing at an arbitrary socket — a proxy in front of the
    /// daemon rather than the daemon itself.
    pub fn for_socket(socket: PathBuf) -> Self {
        CliBuilder { socket }
    }

    /// Build an `assert_cmd::Command` for the logmon-mcp binary with
    /// `LOGMON_BROKER_SOCKET` set so the SDK connects to the test daemon.
    pub fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("logmon-mcp").expect("binary not built");
        cmd.env("LOGMON_BROKER_SOCKET", &self.socket);
        cmd
    }
}

/// A socket in front of `upstream` that adds `"_display": marker` to the
/// result of every reply to one of `methods` — asked for or not.
///
/// **It builds states the real daemon does not produce today.** Two client
/// guards are redundant with the daemon's behaviour, so against the real daemon
/// deleting either one changes nothing a test can see:
///
/// - the CLI's `--json` arm never asks for a rendering, so `emit()`'s own
///   `!json` guard is unreachable unless a reply carries `_display` unasked;
/// - no method has both a renderer and a STRING content field, so the MCP
///   route's body-before-rendering order is unobservable until one does.
///
/// Newline-delimited JSON both ways. The request side records which method each
/// id asked for, before forwarding it, so the reply side can tell which replies
/// to touch; the daemon cannot answer a request it has not yet received, so the
/// record is always there first.
///
/// Returns the tempdir holding the socket (keep it alive) and the socket path.
pub async fn display_injecting_proxy(
    upstream: PathBuf,
    methods: &'static [&'static str],
    marker: &'static str,
) -> (tempfile::TempDir, PathBuf) {
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    let dir = tempfile::tempdir().expect("tempdir for the proxy socket");
    let path = dir.path().join("proxy.sock");
    let listener = UnixListener::bind(&path).expect("bind the proxy socket");

    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(daemon) = UnixStream::connect(&upstream).await else {
                continue;
            };
            let (client_r, mut client_w) = client.into_split();
            let (daemon_r, mut daemon_w) = daemon.into_split();
            let asked: Arc<Mutex<HashMap<u64, String>>> = Arc::default();

            let record = asked.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(client_r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        let id = v.get("id").and_then(Value::as_u64);
                        let method = v.get("method").and_then(Value::as_str);
                        if let (Some(id), Some(method)) = (id, method) {
                            record.lock().unwrap().insert(id, method.to_string());
                        }
                    }
                    let framed = format!("{line}\n");
                    if daemon_w.write_all(framed.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });

            tokio::spawn(async move {
                let mut lines = BufReader::new(daemon_r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut out = line;
                    if let Ok(mut v) = serde_json::from_str::<Value>(&out) {
                        let method = v
                            .get("id")
                            .and_then(Value::as_u64)
                            .and_then(|id| asked.lock().unwrap().get(&id).cloned());
                        let wanted = method.is_some_and(|m| methods.contains(&m.as_str()));
                        if let Some(result) = v.get_mut("result").and_then(Value::as_object_mut) {
                            if wanted {
                                result.insert("_display".to_string(), Value::from(marker));
                                out = v.to_string();
                            }
                        }
                    }
                    let framed = format!("{out}\n");
                    if client_w.write_all(framed.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    (dir, path)
}
