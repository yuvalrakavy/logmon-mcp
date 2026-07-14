//! B2: query responses carry matched (`count`) + `scanned` + buffer stats, so a
//! 0-result is self-diagnosing — "filter's fault" (scanned>0) vs "empty buffer"
//! (scanned==0). See the domains+broker design spec, §B2.
#![cfg(feature = "test-support")]

use logmon_broker_core::gelf::message::Level;
use logmon_broker_core::test_support::*;
use logmon_broker_protocol::LogsRecentResult;
use serde_json::json;

async fn daemon_with_three_logs() -> TestDaemonHandle {
    let daemon = spawn_test_daemon().await;
    daemon.inject_log(Level::Info, "alpha").await;
    daemon.inject_log(Level::Info, "beta").await;
    daemon.inject_log(Level::Error, "gamma").await;
    // Give the log_processor a tick to drain the channel and persist.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    daemon
}

#[tokio::test]
async fn logs_recent_reports_scanned_when_filter_matches_none() {
    let daemon = daemon_with_three_logs().await;
    let mut client = daemon.connect_anon().await;

    // A message filter that matches nothing: data IS flowing, but 0 match.
    let r: LogsRecentResult = client
        .call("logs.recent", json!({ "filter": "m=nonexistent-zzz" }))
        .await
        .unwrap();

    assert_eq!(r.count, 0, "no log matches the filter");
    // The whole buffer was walked (nothing matched, count never reached), so
    // scanned must equal buffer_total and both must cover our 3 injected logs.
    assert!(r.buffer_total >= 3, "buffer_total={}", r.buffer_total);
    assert_eq!(
        r.scanned, r.buffer_total,
        "a no-match filter examines every record: scanned={} buffer_total={}",
        r.scanned, r.buffer_total
    );
    assert!(r.buffer_oldest_seq.is_some());
    assert!(r.buffer_newest_seq.is_some());
}

#[tokio::test]
async fn logs_recent_reports_buffer_stats_on_match() {
    let daemon = daemon_with_three_logs().await;
    let mut client = daemon.connect_anon().await;

    let r: LogsRecentResult = client
        .call("logs.recent", json!({ "count": 10 }))
        .await
        .unwrap();

    assert!(r.buffer_total >= 3);
    assert_eq!(r.count, r.buffer_total, "no filter → every record matches");
    assert_eq!(r.scanned, r.buffer_total, "no filter → every record scanned");
}
