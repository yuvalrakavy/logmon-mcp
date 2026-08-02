//! The MCP tool-call path, driven end to end over real stdio.
//!
//! **This file exists because a mutation pass found the path had no coverage at
//! all.** The entire forwarding closure in `server.rs` — the broker call, the
//! local-output-path strip, the file writes, the error mapping — could be
//! replaced with a canned success and all 1008 tests still passed. Nothing
//! constructed the server or invoked a route; `forwarding_route_tests` only
//! inspected the metadata a route publishes.
//!
//! It could not be reached any other way: `logmon-mcp` is a bin-only crate, so
//! an integration test cannot import `GelfMcpServer`, and nothing depends on it
//! so no other crate's tests reach it either. Speaking the protocol to the real
//! binary is both the only route in and the closest thing to what a client does.
//!
//! Two hazards this harness has to avoid, both learned the hard way:
//!
//! * **`LOGMON_CONFIG_DIR` is set to a scratch directory.** MCP mode calls
//!   `auto_start::ensure_broker_running`, which reads the *real* config dir and
//!   will SPAWN a broker there if none is running. A test must never write into
//!   a developer's live `~/.config/logmon`, let alone start a daemon in it.
//! * **All stdio work happens on `spawn_blocking`.** `spawn_test_daemon` runs
//!   the daemon in-process on this runtime; a blocking `read_line` on the test
//!   thread starves it and the whole thing deadlocks.

mod cli_common;

use cli_common::spawn_with_cli;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// A live `logmon-mcp` speaking MCP over stdio, already initialized.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(socket: &Path, config_dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_logmon-mcp"))
            // A NAMED session, because collectors refuse an anonymous one —
            // "they outlive a disconnect, and an anonymous session's identity
            // does not". This also exercises a global flag in MCP mode, where
            // argv carries flags and no command.
            .arg("--session")
            .arg("mcp-test")
            .env("LOGMON_BROKER_SOCKET", socket)
            // Never the real one — see the module doc.
            .env("LOGMON_CONFIG_DIR", config_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn logmon-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let out = BufReader::new(child.stdout.take().expect("stdout"));
        let mut m = Mcp {
            child,
            stdin,
            out,
            next_id: 1,
        };

        let r = m.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1" }
            }),
        );
        assert!(r.get("result").is_some(), "initialize failed: {r}");
        m.notify("notifications/initialized");
        m
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").expect("write");
        self.stdin.flush().expect("flush");
    }

    fn notify(&mut self, method: &str) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method }));
    }

    /// Send a request and read until its id comes back, skipping notifications.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));

        for _ in 0..500 {
            let mut line = String::new();
            if self.out.read_line(&mut line).expect("read") == 0 {
                panic!("the shim closed stdout before answering `{method}`");
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                return v;
            }
        }
        panic!("no reply to `{method}`");
    }

    /// `tools/call`, returning the first content block's text.
    fn call(&mut self, name: &str, args: Value) -> Result<String, String> {
        let r = self.request("tools/call", json!({ "name": name, "arguments": args }));
        if let Some(e) = r.get("error") {
            return Err(e.to_string());
        }
        let result = &r["result"];
        // rmcp reports a tool-level failure as isError with the text in content.
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if result["isError"].as_bool() == Some(true) {
            return Err(text);
        }
        Ok(text)
    }

    fn tool_names(&mut self) -> Vec<String> {
        let r = self.request("tools/list", json!({}));
        r["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run blocking MCP work off the runtime, so the in-process daemon keeps
/// serving. `work` receives the shim and a scratch directory it may write into.
async fn with_mcp<F>(work: F)
where
    F: FnOnce(&mut Mcp, &Path) + Send + 'static,
{
    let (daemon, _) = spawn_with_cli().await;
    let socket = daemon.socket_path.clone();
    tokio::task::spawn_blocking(move || {
        let scratch = tempfile::tempdir().expect("tempdir");
        let mut mcp = Mcp::start(&socket, scratch.path());
        work(&mut mcp, scratch.path());
    })
    .await
    .expect("blocking work");
    drop(daemon);
}

/// The shim serves what the daemon declares.
#[tokio::test]
async fn the_router_is_built_from_the_daemons_manifest() {
    with_mcp(|mcp, _| {
        let names = mcp.tool_names();
        assert_eq!(
            names.len(),
            logmon_broker_protocol::mcp_tools::TOOLS.len(),
            "the whole surface, not part of it"
        );
        assert!(names.contains(&"list_collectors".to_string()));
    })
    .await;
}

/// The forwarding closure actually forwards — the mutation that replaced this
/// whole path with a canned success left the old suite green.
///
/// `list_collectors` now comes back RENDERED, which is the shim asking the
/// daemon for a `_display` and printing it. That is a stronger proof of
/// forwarding than parsing the envelope was: a canned success cannot produce a
/// table whose rows came out of the daemon's registry.
#[tokio::test]
async fn a_tool_call_reaches_the_daemon_and_returns_its_reply() {
    with_mcp(|mcp, _| {
        let text = mcp.call("list_collectors", json!({})).expect("call ok");
        assert!(
            text.starts_with("(no collectors)"),
            "an empty registry, from the daemon: {text}"
        );
        // The diagnostics ride along, by the structural rule: everything that is
        // not the record array renders. `count=0` beside `(no collectors)` is
        // redundant, and that is the deliberate price of a rule that cannot omit
        // something by accident.
        assert!(text.contains("reserved_bytes="), "{text}");

        mcp.call(
            "add_collector",
            json!({ "name": "c", "filter": "ALL", "level": "tree" }),
        )
        .expect("armed");

        let text = mcp.call("list_collectors", json!({})).expect("call ok");
        assert!(
            text.contains("| collector |"),
            "the reply should be the rendered table, not an envelope: {text}"
        );
        assert!(text.contains("| c "), "the arm must be visible: {text}");
        assert!(
            !text.trim_start().starts_with('{'),
            "the shim printed the envelope rather than the rendering: {text}"
        );
    })
    .await;
}

/// A tool with no renderer still returns its envelope, unchanged.
///
/// This is the incrementality mechanism seen from the client: `add_collector`
/// is a mutation, so by §5's rule it stays JSON, and the shim's fallback is what
/// makes that work without any per-tool knowledge.
#[tokio::test]
async fn a_tool_without_a_renderer_still_returns_json() {
    with_mcp(|mcp, _| {
        let text = mcp
            .call(
                "add_collector",
                json!({ "name": "m", "filter": "ALL", "level": "tree" }),
            )
            .expect("armed");
        let v: Value = serde_json::from_str(&text).expect("an unrendered reply is JSON");
        assert_eq!(v["name"], "m", "{text}");
        assert!(
            v.get("_display").is_none(),
            "a mutation asked for a rendering it does not have: {text}"
        );
    })
    .await;
}

/// Arguments reach the daemon with their types intact, nested objects included.
#[tokio::test]
async fn arguments_survive_the_trip_with_their_types() {
    with_mcp(|mcp, _| {
        mcp.call(
            "add_collector",
            json!({
                "name": "t", "filter": "ALL", "level": "tree",
                "threshold": {
                    "metric": "avg_ms", "op": "gte", "value": 250.0, "window_ms": 60000
                }
            }),
        )
        .expect("armed with a threshold");

        // `get_collector` renders now, so the round trip is read out of the
        // rendering rather than the envelope. That is the stronger assertion:
        // it proves the nested object survived the trip AND that the renderer
        // does not drop it — which it did, until a backstop was added, because
        // the deleted CLI never printed the threshold either.
        let text = mcp.call("get_collector", json!({ "name": "t" })).unwrap();
        assert!(text.contains("avg_ms"), "the metric did not survive: {text}");
        assert!(text.contains("60000"), "the window did not survive: {text}");
        assert!(text.contains("gte"), "the operator did not survive: {text}");
    })
    .await;
}

/// **A misspelled argument is refused, not dropped.** Phase 0 closed this with
/// `deny_unknown_fields` on the hand-written param structs; deleting them
/// reopened it on all 45 tools, and the daemon reads parameters by key, so a
/// forwarded typo is silently ignored and the tool answers a different question.
#[tokio::test]
async fn an_unknown_argument_is_refused_rather_than_silently_dropped() {
    with_mcp(|mcp, _| {
        let err = mcp
            .call("list_collectors", json!({ "nonsense": 1 }))
            .expect_err("must be refused");
        assert!(err.contains("nonsense"), "and must name it: {err}");

        let err = mcp
            .call(
                "add_collector",
                json!({ "name": "x", "filter": "ALL", "group_key": ["a"] }),
            )
            .expect_err("must be refused");
        assert!(err.contains("group_key"), "{err}");
    })
    .await;
}

/// The local file write, driven only by the manifest hint. Neither front-end's
/// use of `files_to_write` had any end-to-end coverage.
#[tokio::test]
async fn a_tool_that_writes_a_local_file_writes_it() {
    with_mcp(|mcp, scratch| {
        let path = scratch.join("export.json");
        let text = mcp
            .call(
                "export_logs",
                json!({ "path": path.to_str().unwrap(), "count": 5 }),
            )
            .expect("exported");

        assert!(path.exists(), "no file was written: {text}");
        let body = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&body).expect("the file holds JSON");
        assert!(
            v.is_array(),
            "the entries, not the envelope — `buffer_total` and friends belong \
             in the reply the caller reads: {body}"
        );
        assert!(text.contains("wrote"), "{text}");
    })
    .await;
}

/// With no path, a document tool returns the DOCUMENT, not an envelope.
///
/// Handing an agent `{"content":"# Title\n…","sidecar_content":{…}}` costs it a
/// turn to unwrap and JSON-escapes every line of the markdown it asked for.
/// The CLI got this in `d0d8b7e`; the MCP surface did not.
#[tokio::test]
async fn a_document_with_no_path_returns_markdown_not_an_envelope() {
    with_mcp(|mcp, _| {
        mcp.call(
            "add_collector",
            json!({ "name": "e", "filter": "ALL", "level": "tree" }),
        )
        .expect("armed");
        mcp.call(
            "snapshot_collector",
            json!({ "name": "e", "label": "base" }),
        )
        .expect("snapshotted");

        let text = mcp
            .call("document_collectors", json!({ "names": ["e@base"] }))
            .expect("documented");

        assert!(
            text.starts_with("---"),
            "the markdown itself, not a JSON wrapper: {}",
            &text[..text.len().min(90)]
        );
        assert!(
            !text.contains("\\n"),
            "and not JSON-escaped: {}",
            &text[..text.len().min(90)]
        );
    })
    .await;
}

/// `collectors document` produces a document AND a companion file beside it.
/// The generic path wrote neither until `d0d8b7e`, and nothing would have
/// caught it: this is the only test that runs the write.
#[tokio::test]
async fn a_document_and_its_companion_both_land_on_disk() {
    with_mcp(|mcp, scratch| {
        let dir = scratch.join("docs");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.md");

        mcp.call(
            "add_collector",
            json!({ "name": "d", "filter": "ALL", "level": "tree" }),
        )
        .expect("armed");
        mcp.call(
            "snapshot_collector",
            json!({ "name": "d", "label": "base" }),
        )
        .expect("snapshotted");

        let text = mcp
            .call(
                "document_collectors",
                json!({ "names": ["d@base"], "path": path.to_str().unwrap() }),
            )
            .expect("documented");

        assert!(path.exists(), "the document was not written: {text}");
        let doc = std::fs::read_to_string(&path).unwrap();
        assert!(
            doc.starts_with("---"),
            "markdown written verbatim, not JSON-encoded: {}",
            &doc[..doc.len().min(80)]
        );

        let others: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "report.md")
            .collect();
        assert_eq!(
            others.len(),
            1,
            "the companion file the document's front-matter names must land \
             beside it, or the reference points at nothing: {others:?}"
        );
    })
    .await;
}
