//! A session's removal resizes the pre-trigger buffer (gh #25).
//!
//! The buffer holds the last N arrivals, N being the largest `pre_window` among the domain's
//! sessions. A trigger firing on a traced record also stores that trace's entries still in the
//! buffer, so N is how far back such a firing reaches. Each test arranges a traced record 1,000
//! arrivals before an ERROR: inside a buffer sized 3,000 by a big `pre_window`, outside the 500
//! the remaining session's default `l>=ERROR` trigger needs. Whether the early record is stored
//! says which size the buffer had.
#![cfg(feature = "test-support")]

use logmon_broker_core::gelf::message::{Level, LogEntry};
use logmon_broker_core::test_support::*;
use serde_json::{json, Value};
use std::time::Duration;

const TRACE: u128 = 0xabcdef;

fn traced(level: Level, msg: &str) -> LogEntry {
    let mut e = LogEntry::synthetic(level, msg);
    e.trace_id = Some(TRACE);
    e
}

/// A session that narrows storage (so records are stored only by a trigger), with the default
/// `l>=ERROR` trigger (`pre_window` 500).
async fn keeper(daemon: &TestDaemonHandle) -> TestClient {
    let mut k = daemon.connect_named("keeper", None).await;
    let _: Value = k
        .call("filters.add", json!({ "filter": "m=__nothing_matches__" }))
        .await
        .unwrap();
    k
}

/// Add a trigger that never fires but sizes the buffer to 3,000.
async fn add_big_window(client: &mut TestClient) {
    let _: Value = client
        .call(
            "triggers.add",
            json!({ "filter": "m=__never_fires__", "pre_window": 3000 }),
        )
        .await
        .unwrap();
}

/// The early traced record, 1,000 untraced records, then a traced ERROR. Returns the messages
/// stored for the trace once the ERROR's firing has stored it.
async fn run_scenario(daemon: &TestDaemonHandle, reader: &mut TestClient) -> Vec<String> {
    daemon.inject_entry(traced(Level::Info, "early")).await;
    for i in 0..1000 {
        daemon.inject_log(Level::Info, &format!("filler {i}")).await;
    }
    daemon.inject_entry(traced(Level::Error, "boom")).await;
    let trace_hex = format!("{TRACE:032x}");
    for _ in 0..500 {
        let r: Value = reader
            .call("logs.recent", json!({ "trace_id": trace_hex }))
            .await
            .unwrap();
        let msgs: Vec<String> = r["logs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["message"].as_str().unwrap().to_string())
            .collect();
        if msgs.iter().any(|m| m == "boom") {
            return msgs;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the ERROR was never stored");
}

/// Wait until the daemon lists exactly `n` sessions.
async fn wait_for_sessions(client: &mut TestClient, n: usize) {
    for _ in 0..500 {
        let r: Value = client.call("sessions.list", json!({})).await.unwrap();
        if r["sessions"].as_array().unwrap().len() == n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the daemon never listed {n} sessions");
}

/// The instrument fires: while the big-window session exists, the early record is in the
/// buffer and the firing stores it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_big_window_reaches_the_early_record() {
    let daemon = spawn_test_daemon().await;
    let mut k = keeper(&daemon).await;
    let mut big = daemon.connect_named("big", None).await;
    add_big_window(&mut big).await;
    assert_eq!(run_scenario(&daemon, &mut k).await, vec!["early", "boom"]);
}

/// `sessions.drop` removes the big window's session, and the buffer shrinks with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_session_no_longer_sizes_the_buffer() {
    let daemon = spawn_test_daemon().await;
    let mut k = keeper(&daemon).await;
    let mut big = daemon.connect_named("big", None).await;
    add_big_window(&mut big).await;
    big.close().await.unwrap();
    wait_for_disconnected(&mut k, "big").await;
    let _: Value = k
        .call("sessions.drop", json!({ "name": "big" }))
        .await
        .unwrap();
    assert_eq!(run_scenario(&daemon, &mut k).await, vec!["boom"]);
}

/// An anonymous session is removed when it disconnects, and the buffer shrinks with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disconnected_anonymous_session_no_longer_sizes_the_buffer() {
    let daemon = spawn_test_daemon().await;
    let mut k = keeper(&daemon).await;
    let mut big = daemon.connect_anon().await;
    add_big_window(&mut big).await;
    wait_for_sessions(&mut k, 2).await;
    big.close().await.unwrap();
    wait_for_sessions(&mut k, 1).await;
    assert_eq!(run_scenario(&daemon, &mut k).await, vec!["boom"]);
}

/// A session that CONNECTS sizes the buffer with its own triggers. Here the only other session
/// keeps a 10-record trigger, so the buffer is 10; a new session's default `l>=ERROR` trigger
/// (500) must reach a record 100 arrivals back — it used to get 10 until some trigger or filter
/// change resynced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connecting_session_sizes_the_buffer_with_its_own_triggers() {
    let daemon = spawn_test_daemon().await;
    let mut f = daemon.connect_named("filterer", None).await;
    let _: Value = f
        .call("filters.add", json!({ "filter": "m=__nothing_matches__" }))
        .await
        .unwrap();
    let listed: Value = f.call("triggers.list", json!({})).await.unwrap();
    for t in listed["triggers"].as_array().unwrap() {
        let _: Value = f
            .call("triggers.remove", json!({ "id": t["id"] }))
            .await
            .unwrap();
    }
    let _: Value = f
        .call(
            "triggers.add",
            json!({ "filter": "m=__never_fires__", "pre_window": 10 }),
        )
        .await
        .unwrap();

    // A new session, defaults only — it changes no trigger and no filter.
    let mut b = daemon.connect_anon().await;
    daemon.inject_log(Level::Warn, "early-warn").await;
    for i in 0..100 {
        daemon.inject_log(Level::Info, &format!("filler {i}")).await;
    }
    daemon.inject_log(Level::Error, "boom").await;
    for _ in 0..500 {
        let r: Value = b
            .call("logs.recent", json!({ "filter": "m=boom" }))
            .await
            .unwrap();
        if r["count"].as_u64().unwrap() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let r: Value = b
        .call("logs.recent", json!({ "filter": "m=early-warn" }))
        .await
        .unwrap();
    assert_eq!(
        r["count"],
        json!(1),
        "the new session's pre-window reached it: {r}"
    );
}

/// A session renamed onto a stale holder's name displaces it, triggers and all, and the buffer
/// shrinks with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_displaced_by_a_rename_no_longer_sizes_the_buffer() {
    let daemon = spawn_test_daemon().await;
    let mut k = keeper(&daemon).await;
    let mut big = daemon.connect_named("big", None).await;
    add_big_window(&mut big).await;
    big.close().await.unwrap();
    wait_for_disconnected(&mut k, "big").await;
    let _: Value = k
        .call("sessions.rename", json!({ "name": "big" }))
        .await
        .unwrap();
    assert_eq!(run_scenario(&daemon, &mut k).await, vec!["boom"]);
}

/// Wait until the named session `name` is listed as disconnected (`sessions.drop` refuses a
/// connected one).
async fn wait_for_disconnected(client: &mut TestClient, name: &str) {
    for _ in 0..500 {
        let r: Value = client.call("sessions.list", json!({})).await.unwrap();
        let gone = r["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == name && s["connected"] == false);
        if gone {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("session {name} never disconnected");
}
