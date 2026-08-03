//! `status.get` states what a shim of this broker's version exposes, so a shim
//! too old to compute the gap itself still renders the facts (design §3.0/§3.3).
//!
//! The channel matters more than the fields: `get_status` relays the daemon's
//! JSON verbatim and always has, so this is the only response that reaches an
//! installation already in the field.
#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::mcp_tools;
use logmon_broker_protocol::StatusGetResult;
use serde_json::{json, Value};

#[tokio::test]
async fn status_states_the_broker_version_and_the_tools_it_supports() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let status: Value = client.call("status.get", json!({})).await.unwrap();
    assert_eq!(
        status["broker_version"],
        env!("CARGO_PKG_VERSION"),
        "the version a reader is told to upgrade *to*"
    );

    let tools: Vec<String> = status["broker_tools"]
        .as_array()
        .expect("broker_tools must be an array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        tools,
        mcp_tools::tool_names(),
        "the wire list must be derived from the shared table, never hand-written"
    );
    assert!(
        tools.contains(&"snapshot_collector".to_string()),
        "tool names, not RPC method names — an agent holds the former"
    );
}

/// The typed SDK path deserializes `StatusGetResult` and re-serializes it, so a
/// field the daemon emits but the struct lacks is silently dropped for every
/// typed caller. That has happened in this crate before (`post_remaining`), and
/// `verify-schema` cannot catch it: it checks schema-against-Rust, not
/// daemon-JSON-against-Rust. This is the check that can.
#[tokio::test]
async fn the_new_fields_survive_the_typed_result_struct() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let typed: StatusGetResult = client.call("status.get", json!({})).await.unwrap();
    assert_eq!(typed.broker_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(typed.broker_tools, mcp_tools::tool_names());
}

/// The known hole in the channel, pinned so it is a recorded limitation rather
/// than a discovery.
///
/// `handle_status` resolves the caller's domain first, so once that domain is
/// deleted the whole call errors and the skew facts go with it — the only
/// field-reaching channel goes dark in a confused state. Pre-existing, not
/// introduced here.
///
/// Left unfixed deliberately: reaching it takes `use_domain(x)` then
/// `delete_domain(x)`, the error names its own remedy, and serving a partial
/// status would mean defaulting `StatusGetResult::store` — letting an absent
/// buffer read as an empty one. If this ever bites in practice, the fix is a
/// partial-status path, not a defaulted field.
#[tokio::test]
async fn a_deleted_domain_takes_the_skew_facts_down_with_it() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let _: Value = client
        .call("domains.create", json!({ "name": "doomed" }))
        .await
        .unwrap();
    let _: Value = client
        .call("domains.use", json!({ "name": "doomed" }))
        .await
        .unwrap();
    let _: Value = client
        .call("domains.delete", json!({ "name": "doomed" }))
        .await
        .unwrap();

    let err = client
        .call::<Value>("status.get", json!({}))
        .await
        .expect_err("documented limitation: status.get errors on a deleted domain");
    assert!(
        err.to_string().contains("use_domain to rebind"),
        "and the error must keep naming its own remedy: {err}"
    );
}

/// **No key the daemon sends is silently dropped by the typed result struct.**
///
/// The class, not the instance. `crates/protocol/src/methods.rs:432` records
/// this repo emitting `post_remaining` on the wire while the type lacked it —
/// serde dropped it for every typed SDK caller and nothing noticed, because
/// `verify-schema` compares schema-against-Rust rather than
/// daemon-JSON-against-Rust.
///
/// A mutation proved the gap was still open: with the one test line that *names*
/// `broker_tools` removed, deleting the field compiled clean and every suite in
/// the workspace passed. Only the compile-time coupling of that line stood in
/// the way — not a designed catch, and no protection at all for the next field
/// someone adds to `handle_status`.
///
/// This compares key sets directly, so it guards every future field too.
#[tokio::test]
async fn the_typed_status_struct_drops_no_key_the_daemon_sends() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let raw: Value = client.call("status.get", json!({})).await.unwrap();
    let typed: StatusGetResult = serde_json::from_value(raw.clone()).unwrap();
    let round_tripped = serde_json::to_value(&typed).unwrap();

    let sent: Vec<&String> = raw.as_object().unwrap().keys().collect();
    let kept = round_tripped.as_object().unwrap();
    let dropped: Vec<&&String> = sent.iter().filter(|k| !kept.contains_key(**k)).collect();

    assert!(
        dropped.is_empty(),
        "the daemon sends {dropped:?}, which StatusGetResult does not carry — \
         every typed SDK caller loses them silently. Mirror them on the struct \
         (see the `post_remaining` note in protocol/src/methods.rs)."
    );
}

/// Every method in the shared table is one this daemon actually dispatches.
///
/// Asserts on the *message*, not the code: every handler failure maps to
/// -32601, so a code check would pass for a method that does not exist. A known
/// method failing on missing params returns a different message and passes,
/// which is what makes this safe to run across mutating methods — the daemon is
/// a throwaway and the assertion does not require success.
#[tokio::test]
async fn every_method_in_the_shared_table_is_dispatched_by_this_daemon() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let mut unknown = Vec::new();
    let mut probed = 0usize;
    for mcp_tools::Tool {
        name: tool, method, ..
    } in mcp_tools::TOOLS
    {
        let res: Result<Value, _> = client.call(method, json!({})).await;
        probed += 1;
        if let Err(e) = res {
            let msg = e.to_string();
            if msg.contains("unknown method") {
                unknown.push(format!("{tool} -> {method}"));
            }
            // A transport failure means the connection died mid-loop — every
            // probe after it would "pass" without checking anything, and this
            // test would silently go vacuous for the tail of the table.
            assert!(
                !msg.contains("connection") && !msg.contains("closed"),
                "the probe connection died at `{method}`, so nothing after it was checked: {msg}"
            );
        }
    }
    assert_eq!(
        probed,
        mcp_tools::TOOLS.len(),
        "every row must be probed, or absence of failures proves nothing"
    );
    assert!(
        unknown.is_empty(),
        "the table names methods this daemon does not have: {unknown:?}"
    );
}

/// T1 (design §8) — the daemon SENDS the skill, not merely declares it.
///
/// `ToolsManifestResult` is schema-only: never constructed, never deserialized.
/// The reply is a hand-built `json!` literal, so a field added to that struct
/// changes the published schema and not one byte of what goes on the wire.
/// `verify-schema` compares the schema against Rust and cannot see that gap;
/// this is the only check that can.
///
/// It matters because the shim no longer carries an embedded copy to fall back
/// on — an absent field means every client starts with no guidance at all.
#[tokio::test]
async fn tools_manifest_serves_the_skill_document() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let reply: Value = client.call("tools.manifest", json!({})).await.unwrap();

    let skill = reply["skill"].as_str().unwrap_or_else(|| {
        panic!(
            "`tools.manifest` must carry `skill`. The struct that declares it \
             does not build this reply, so adding the field there is not enough \
             — it also belongs in the `json!` in `handle_tools_manifest`."
        )
    });

    assert_eq!(
        skill,
        mcp_tools::SKILL,
        "served verbatim from the shared const, never re-rendered"
    );
    assert!(
        skill.len() > 500,
        "an empty or truncated document silently removes every piece of guidance \
         a client would have surfaced (got {} bytes)",
        skill.len()
    );
}
