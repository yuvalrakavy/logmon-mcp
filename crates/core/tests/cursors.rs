//! End-to-end tests for the seq-based bookmark protocol introduced by the
//! cursor design (Task 2). The cursor primitive itself (`c>=`) ships in a
//! later task; this file only exercises the storage + protocol + handler
//! refactor.

#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{BookmarksAddResult, BookmarksListResult};
use serde_json::json;
use std::fs;

#[tokio::test]
async fn add_bookmark_with_explicit_start_seq() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let added: BookmarksAddResult = client
        .call(
            "bookmarks.add",
            json!({
                "name": "anchor",
                "start_seq": 42,
                "description": "explicit start",
            }),
        )
        .await
        .unwrap();
    assert_eq!(added.seq, 42);

    let list: BookmarksListResult = client.call("bookmarks.list", json!({})).await.unwrap();
    let entry = list
        .bookmarks
        .iter()
        .find(|b| b.qualified_name.ends_with("/anchor"))
        .expect("expected to find /anchor in list");
    assert_eq!(entry.seq, 42);
    assert_eq!(entry.description.as_deref(), Some("explicit start"));
}

#[tokio::test]
async fn add_bookmark_replace_overwrites() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let _: BookmarksAddResult = client
        .call("bookmarks.add", json!({ "name": "x", "start_seq": 1 }))
        .await
        .unwrap();

    // Without `replace: true`, a duplicate must error.
    let err: Result<BookmarksAddResult, _> = client
        .call("bookmarks.add", json!({ "name": "x", "start_seq": 2 }))
        .await;
    assert!(err.is_err());

    // With `replace: true`, the same name overwrites at a fresh seq.
    let _: BookmarksAddResult = client
        .call(
            "bookmarks.add",
            json!({ "name": "x", "start_seq": 2, "replace": true }),
        )
        .await
        .unwrap();

    let list: BookmarksListResult = client.call("bookmarks.list", json!({})).await.unwrap();
    let entry = list
        .bookmarks
        .iter()
        .find(|b| b.qualified_name.ends_with("/x"))
        .expect("expected to find /x in list");
    assert_eq!(entry.seq, 2);
}

#[tokio::test]
async fn legacy_state_json_with_old_bookmark_shape_warns_and_continues() {
    // Spawn a daemon, get its tempdir, shut down, write a legacy-shape state.json,
    // restart, verify daemon starts and bookmarks are dropped.
    let daemon = spawn_test_daemon().await;
    let tempdir = daemon.tempdir.clone();
    let tempdir_path = tempdir.path().to_path_buf();
    daemon.shutdown().await;
    drop(daemon);

    let legacy = json!({
        "seq_block": 1000,
        "named_sessions": {
            "test_session": {
                "triggers": [],
                "filters": [],
                "client_info": null,
                "bookmarks": [{
                    "name": "old",
                    "timestamp": "2026-04-30T12:00:00Z",
                    "description": "from before cursor migration"
                }]
            }
        }
    });
    fs::write(
        tempdir_path.join("state.json"),
        serde_json::to_string_pretty(&legacy).unwrap(),
    )
    .unwrap();

    // Re-spawn with same tempdir using the single-arg public API.
    let daemon = TestDaemonHandle::spawn_in_tempdir(tempdir).await;
    let mut client = daemon.connect_named("test_session", None).await;

    // Daemon survived deserialize; bookmarks should be empty for the restored session.
    let list: BookmarksListResult = client.call("bookmarks.list", json!({})).await.unwrap();
    assert!(
        list.bookmarks
            .iter()
            .all(|b| !b.qualified_name.starts_with("test_session/old")),
        "legacy bookmarks should have been discarded; got {:?}",
        list.bookmarks
    );
}

#[tokio::test]
async fn bookmark_persists_across_restart() {
    // Verify the persistence round-trip works for seq-based bookmarks via the
    // pure-read b>= path. Cursor-mode cross-restart behavior is verified by
    // cursor_persists_across_restart_returns_post_restart_records_only below.
    let mut daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_named("persist", None).await;

    // Capture pre-add and post-add times to verify created_at survives the restart.
    let pre_add = chrono::Utc::now();
    let added: BookmarksAddResult = client
        .call(
            "bookmarks.add",
            json!({
                "name": "anchor",
                "start_seq": 42,
                "description": "preserved",
            }),
        )
        .await
        .unwrap();
    let post_add = chrono::Utc::now();
    assert_eq!(added.seq, 42);

    // Drop client, restart daemon, reconnect to same named session.
    drop(client);
    daemon.restart().await;
    let mut client = daemon.connect_named("persist", None).await;

    // Bookmark survives with original seq, description, AND created_at.
    let list: BookmarksListResult = client.call("bookmarks.list", json!({})).await.unwrap();
    let entry = list
        .bookmarks
        .iter()
        .find(|b| b.qualified_name == "persist/anchor")
        .expect("anchor bookmark should survive restart");
    assert_eq!(entry.seq, 42);
    assert_eq!(entry.description.as_deref(), Some("preserved"));
    assert!(
        entry.created_at >= pre_add && entry.created_at <= post_add,
        "created_at should be preserved from pre-restart; got {:?}, pre_add {:?}, post_add {:?}",
        entry.created_at,
        pre_add,
        post_add
    );
}

#[tokio::test]
async fn c_ge_rejected_in_traces_recent() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let result: Result<serde_json::Value, _> = client
        .call(
            "traces.recent",
            json!({
                "filter": "c>=mycur"
            }),
        )
        .await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cursor qualifier not permitted"), "got: {err}");
}

#[tokio::test]
async fn c_ge_rejected_in_traces_get() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let result: Result<serde_json::Value, _> = client
        .call(
            "traces.get",
            json!({
                "trace_id": "00000000000000000000000000000001",
                "filter": "c>=mycur"
            }),
        )
        .await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cursor qualifier not permitted"), "got: {err}");
}

#[tokio::test]
async fn c_ge_rejected_in_traces_slow() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let result: Result<serde_json::Value, _> = client
        .call(
            "traces.slow",
            json!({
                "filter": "c>=mycur"
            }),
        )
        .await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cursor qualifier not permitted"), "got: {err}");
}

#[tokio::test]
async fn c_ge_rejected_in_logs_recent_with_trace_id() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let result: Result<serde_json::Value, _> = client
        .call(
            "logs.recent",
            json!({
                "trace_id": "0123456789abcdef0123456789abcdef",
                "filter": "c>=mycur"
            }),
        )
        .await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cursor qualifier not permitted"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Tasks 10/11/12: cursor commit + oldest-first ordering through the allow-list
// handlers (logs.recent / logs.export / traces.logs).
// ---------------------------------------------------------------------------

use logmon_broker_core::gelf::message::Level;
use logmon_broker_protocol::{LogsExportResult, LogsRecentResult, TracesLogsResult};

#[tokio::test]
async fn cursor_advances_and_paginates_oldest_first() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    for i in 0..5 {
        daemon.inject_log(Level::Info, &format!("record-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // First cursor read with count=3 — returns oldest 3, advances cursor.
    let r1: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "count": 3,
                "filter": "c>=cur",
            }),
        )
        .await
        .unwrap();
    assert_eq!(r1.logs.len(), 3);
    assert_eq!(r1.logs[0].message, "record-0"); // oldest first
    assert_eq!(r1.logs[1].message, "record-1");
    assert_eq!(r1.logs[2].message, "record-2");
    assert!(r1.cursor_advanced_to.is_some());

    // Second cursor read — returns next 2.
    let r2: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "count": 3,
                "filter": "c>=cur",
            }),
        )
        .await
        .unwrap();
    assert_eq!(r2.logs.len(), 2);
    assert_eq!(r2.logs[0].message, "record-3");
    assert_eq!(r2.logs[1].message, "record-4");

    // Third cursor read — empty, no advance.
    let r3: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "count": 3,
                "filter": "c>=cur",
            }),
        )
        .await
        .unwrap();
    assert!(r3.logs.is_empty());
    assert_eq!(r3.cursor_advanced_to, None);
}

#[tokio::test]
async fn no_cursor_returns_newest_first_unchanged() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    for i in 0..3 {
        daemon.inject_log(Level::Info, &format!("record-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let r: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "count": 5,
            }),
        )
        .await
        .unwrap();
    assert_eq!(r.logs.len(), 3);
    assert_eq!(r.logs[0].message, "record-2"); // newest first
    assert_eq!(r.logs[2].message, "record-0");
    assert_eq!(r.cursor_advanced_to, None);
}

#[tokio::test]
async fn export_with_cursor_advances_and_returns_oldest_first() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    for i in 0..5 {
        daemon.inject_log(Level::Info, &format!("export-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let r: LogsExportResult = client
        .call(
            "logs.export",
            json!({
                "filter": "c>=expcur",
                "count": 100,
            }),
        )
        .await
        .unwrap();
    assert_eq!(r.logs.len(), 5);
    assert_eq!(r.logs[0].message, "export-0");
    assert!(r.cursor_advanced_to.is_some());

    let r2: LogsExportResult = client
        .call(
            "logs.export",
            json!({
                "filter": "c>=expcur",
                "count": 100,
            }),
        )
        .await
        .unwrap();
    assert!(r2.logs.is_empty());
    assert_eq!(r2.cursor_advanced_to, None);
}

#[tokio::test]
async fn traces_logs_with_cursor_field_present() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // Use a trace_id that no logs have. Result is empty; cursor_advanced_to is None.
    // The point of this test is just that the field exists on the wire.
    let r: TracesLogsResult = client
        .call(
            "traces.logs",
            json!({
                "trace_id": "00000000000000000000000000000001",
                "filter": "c>=tlcur",
            }),
        )
        .await
        .unwrap();
    assert!(r.logs.is_empty());
    assert_eq!(r.cursor_advanced_to, None);
}

// ---------------------------------------------------------------------------
// Task 13: anonymous session disconnect drops bookmarks
// ---------------------------------------------------------------------------

#[test]
fn bookmark_store_clear_session_removes_matching() {
    use logmon_broker_core::store::bookmarks::BookmarkStore;
    let store = BookmarkStore::new();

    let (_b1, _) = store.add("session1", "name1", 0, None, false).unwrap();
    let (_b2, _) = store.add("session1", "name2", 1, None, false).unwrap();
    let (_b3, _) = store.add("session2", "name3", 2, None, false).unwrap();

    assert_eq!(store.list().len(), 3);

    // Clear session1
    let removed = store.clear_session("session1");
    assert_eq!(removed, 2);
    assert_eq!(store.list().len(), 1);
    assert_eq!(store.list()[0].qualified_name, "session2/name3");
}

#[tokio::test]
async fn anonymous_session_disconnect_drops_bookmarks() {
    let daemon = spawn_test_daemon().await;

    {
        let mut client = daemon.connect_anon().await;
        let _: BookmarksAddResult = client
            .call(
                "bookmarks.add",
                json!({
                    "name": "ephemeral"
                }),
            )
            .await
            .unwrap();
        // Explicitly close the connection to trigger EOF on the server side
        client.close().await.unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut client = daemon.connect_anon().await;
    let list: BookmarksListResult = client.call("bookmarks.list", json!({})).await.unwrap();
    let bookmarks = &list.bookmarks;
    assert!(
        !bookmarks
            .iter()
            .any(|b| b.qualified_name.ends_with("/ephemeral")),
        "ephemeral bookmark should be dropped on anon disconnect; got: {bookmarks:?}"
    );
}

// ---------------------------------------------------------------------------
// Task 15: Regression tests for cursor eviction under churn
// ---------------------------------------------------------------------------

// Gated until TestDaemonHandle::spawn exposes a buffer_size override.
// Today's spawn() uses a 10_000-record buffer; flooding past it would take
// ~30s of injection at typical rates and would be flaky in CI. Once the
// harness gains a small-buffer mode, drop the #[ignore] and verify.
#[tokio::test]
#[ignore = "needs harness buffer_size override; see TestDaemonHandle::spawn"]
async fn cursor_evicted_under_churn_auto_recreates_with_full_buffer() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    daemon.inject_log(Level::Info, "before").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let _r1: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "filter": "c>=churn", "count": 100
            }),
        )
        .await
        .unwrap();

    for i in 0..15_000 {
        daemon.inject_log(Level::Info, &format!("flood-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let r3: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "filter": "c>=churn", "count": 50_000
            }),
        )
        .await
        .unwrap();
    assert!(
        r3.logs.len() >= 1000,
        "expected flood-recreation to return many records, got {}",
        r3.logs.len()
    );
}

#[tokio::test]
async fn cursor_persists_across_restart_returns_post_restart_records_only() {
    let mut daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_named("cur-persist", None).await;

    // Inject + advance the cursor.
    daemon.inject_log(Level::Info, "before-restart").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r1: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "filter": "c>=mycur", "count": 100
            }),
        )
        .await
        .unwrap();
    assert!(
        r1.cursor_advanced_to.is_some(),
        "first read should advance cursor"
    );
    let advanced_to = r1.cursor_advanced_to.unwrap();
    assert!(advanced_to > 0);

    // Drop client, restart daemon, reconnect.
    drop(client);
    daemon.restart().await;
    let mut client = daemon.connect_named("cur-persist", None).await;

    // The cursor's persisted seq is now far below the new seq counter (which
    // resumed at state.seq_block + SEQ_BLOCK_SIZE). The in-memory buffer is
    // empty post-restart; first c>= read returns nothing.
    let r2: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "filter": "c>=mycur", "count": 100
            }),
        )
        .await
        .unwrap();
    assert!(
        r2.logs.is_empty(),
        "expected empty post-restart, got {:?}",
        r2.logs
    );
    assert_eq!(r2.cursor_advanced_to, None);

    // Inject a new record post-restart; cursor advances from the post-restart seq.
    daemon.inject_log(Level::Info, "after-restart").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r3: LogsRecentResult = client
        .call(
            "logs.recent",
            json!({
                "filter": "c>=mycur", "count": 100
            }),
        )
        .await
        .unwrap();
    assert_eq!(r3.logs.len(), 1);
    assert_eq!(r3.logs[0].message, "after-restart");
    assert!(
        r3.cursor_advanced_to.unwrap() > advanced_to,
        "post-restart cursor seq should exceed pre-restart seq"
    );
}

// ---------------------------------------------------------------------------
// §6.1 — a seq range on logs.export
// ---------------------------------------------------------------------------

/// Both ends are inclusive, and the single-seq range is the case that proves it.
///
/// `SeqOp` has only strict variants, so the range lowers to `Gt(from - 1)` and
/// `Lt(to + 1)`. Had the parameter been left strict to match, a window of "the
/// anchor, plus none before and none after" would exclude the anchor — a
/// permanent off-by-one at the centre of every archived window.
#[tokio::test]
async fn a_seq_range_is_inclusive_at_both_ends() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    for i in 0..6 {
        daemon.inject_log(Level::Info, &format!("range-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let all: LogsExportResult = client
        .call("logs.export", json!({ "count": 100 }))
        .await
        .unwrap();
    let seqs: Vec<u64> = all.logs.iter().map(|l| l.seq).collect();
    assert_eq!(seqs.len(), 6, "six injected: {seqs:?}");
    let (lo, hi) = (
        seqs.iter().min().copied().unwrap(),
        seqs.iter().max().copied().unwrap(),
    );

    // The whole span, named explicitly, must return every entry.
    let whole: LogsExportResult = client
        .call(
            "logs.export",
            json!({ "from_seq": lo, "to_seq": hi, "count": 100 }),
        )
        .await
        .unwrap();
    assert_eq!(
        whole.logs.len(),
        6,
        "an inclusive range must contain both ends"
    );

    // A range of one seq returns exactly that entry — the anchor case.
    let one: LogsExportResult = client
        .call(
            "logs.export",
            json!({ "from_seq": lo, "to_seq": lo, "count": 100 }),
        )
        .await
        .unwrap();
    assert_eq!(one.logs.len(), 1, "from == to must return that one entry");
    assert_eq!(one.logs[0].seq, lo);
}

/// The range narrows, and it narrows from both directions.
#[tokio::test]
async fn a_seq_range_excludes_what_lies_outside_it() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    for i in 0..6 {
        daemon.inject_log(Level::Info, &format!("edge-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let all: LogsExportResult = client
        .call("logs.export", json!({ "count": 100 }))
        .await
        .unwrap();
    let mut seqs: Vec<u64> = all.logs.iter().map(|l| l.seq).collect();
    seqs.sort_unstable();

    let inner: LogsExportResult = client
        .call(
            "logs.export",
            json!({ "from_seq": seqs[1], "to_seq": seqs[4], "count": 100 }),
        )
        .await
        .unwrap();
    let got: Vec<u64> = {
        let mut v: Vec<u64> = inner.logs.iter().map(|l| l.seq).collect();
        v.sort_unstable();
        v
    };
    assert_eq!(
        got,
        seqs[1..=4].to_vec(),
        "four entries, endpoints included"
    );
}

/// `capped` is a fact, not an inference.
///
/// "Asked for N, got N" is true both when the range was cut short and when
/// exactly N existed, so a reader cannot tell them apart — and §4.2 forbids
/// calling a capped range complete. The two cases are asserted side by side
/// because it is the DIFFERENCE that carries the information.
#[tokio::test]
async fn capped_distinguishes_a_cut_range_from_one_that_fit_exactly() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    for i in 0..6 {
        daemon.inject_log(Level::Info, &format!("cap-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let cut: LogsExportResult = client
        .call("logs.export", json!({ "count": 4 }))
        .await
        .unwrap();
    assert_eq!(cut.logs.len(), 4);
    assert!(cut.capped, "four of six is capped: {cut:?}");

    let exact: LogsExportResult = client
        .call("logs.export", json!({ "count": 6 }))
        .await
        .unwrap();
    assert_eq!(exact.logs.len(), 6);
    assert!(
        !exact.capped,
        "six of six is not capped, and that is the case an inference gets wrong: {exact:?}"
    );

    let roomy: LogsExportResult = client
        .call("logs.export", json!({ "count": 100 }))
        .await
        .unwrap();
    assert!(!roomy.capped, "{roomy:?}");
}

/// The range reaches the eviction detector, which is the whole reason it is
/// lowered into the filter instead of carried beside it.
///
/// `evicted_before_window` reads its lower bound out of the PARSED FILTER via
/// `resolved_lower_bound`, matching only `Qualifier::SeqFilter { op: Gt }`. A
/// `from_seq` carried as a loose parameter would never reach it, so a window
/// starting below what the ring still holds would come back `truncated: false`
/// — and a reader deciding whether that window is complete would be told it is.
#[tokio::test]
async fn a_range_starting_below_the_buffer_reports_eviction() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    for i in 0..4 {
        daemon.inject_log(Level::Info, &format!("evict-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let all: LogsExportResult = client
        .call("logs.export", json!({ "count": 100 }))
        .await
        .unwrap();
    let oldest = all
        .buffer_oldest_seq
        .expect("a non-empty buffer has an oldest");

    // Deliberately well below what the ring holds.
    let below: LogsExportResult = client
        .call(
            "logs.export",
            json!({ "from_seq": oldest.saturating_sub(50), "count": 100 }),
        )
        .await
        .unwrap();
    assert!(
        below.truncated,
        "a window starting below the retained buffer is not complete: {below:?}"
    );
    assert!(
        below.evicted_before_window.is_some(),
        "and the gap must be named, not merely flagged: {below:?}"
    );
}
