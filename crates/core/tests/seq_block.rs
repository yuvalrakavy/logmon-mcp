//! Seq-block high-water fix (spec §8, decision #2).
//!
//! The boot-time seq reservation is a FIXED `initial_seq + SEQ_BLOCK_SIZE`
//! (1000) step. A run that emits more than 1000 records advances the live seq
//! counter past that reservation; if shutdown persists only the fixed
//! reservation, the next boot RE-USES seqs already handed out — a persisted
//! cursor at one of those seqs would then alias a brand-new record. The fix
//! persists `max(reserved_block, counter.current())` so seqs never rewind.
#![cfg(feature = "test-support")]

use logmon_broker_core::gelf::message::Level;
use logmon_broker_core::test_support::*;
use logmon_broker_protocol::LogsRecentResult;
use serde_json::json;

async fn newest_seq(daemon: &TestDaemonHandle) -> u64 {
    let mut client = daemon.connect_anon().await;
    let r: LogsRecentResult = client
        .call("logs.recent", json!({ "count": 1 }))
        .await
        .unwrap();
    r.buffer_newest_seq.expect("buffer is non-empty")
}

#[tokio::test]
async fn seqs_do_not_rewind_across_restart_after_a_large_run() {
    let mut daemon = spawn_test_daemon().await;

    // A run that emits well past SEQ_BLOCK_SIZE (1000).
    for i in 0..1500 {
        daemon.inject_log(Level::Info, &format!("line {i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let high_water = newest_seq(&daemon).await;
    assert!(
        high_water > 1000,
        "sanity: high-water {high_water} should exceed the 1000-seq reservation"
    );

    // Kill + restart, preserving state.json (this is what persists seq_block).
    daemon.restart().await;

    // A single record after restart. Its seq must be ABOVE the pre-restart
    // high-water — otherwise a persisted cursor at a pre-restart seq would
    // alias this new record.
    daemon.inject_log(Level::Info, "after restart").await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let after = newest_seq(&daemon).await;
    assert!(
        after > high_water,
        "seq rewound across restart: pre={high_water} post={after} — persisted \
         cursors would alias new records"
    );
}
