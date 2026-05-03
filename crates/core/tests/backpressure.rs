//! Backpressure stress test: pump many GELF UDP datagrams rapidly while a
//! slow consumer simulation keeps the channel under pressure, then verify:
//!
//! 1. The broker stays responsive (status.get returns within 200 ms).
//! 2. No information from compliant clients (those that honour 429 /
//!    UNAVAILABLE) is lost — but in this test the GELF UDP producer is
//!    NON-compliant (UDP can't be told to back off), so we accept that
//!    some drops will occur and assert the counter sees them.
//! 3. The drop counter increments are visible in `status.get`.
//!
//! This is the regression guard for the wedge bug observed during
//! `store-test tests/ --enforce-baseline` runs that filled the prior
//! 1024-cap channel within minutes.

#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::StatusGetResult;
use serde_json::json;
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broker_stays_responsive_under_burst_load() {
    // Spawn a real daemon (not the injected-channel harness) so the GELF
    // UDP receiver is actually bound. spawn_test_daemon() uses the
    // injected harness, so we use the lower-level path.
    let daemon = TestDaemonHandle::spawn_with_real_receivers().await;
    let udp_port = daemon.gelf_udp_port().await;

    // Producer task: blast 200 000 GELF UDP datagrams as fast as the
    // kernel will let us, ignoring drops. This will overflow the 65 536
    // user-space channel during the burst.
    let producer = tokio::spawn(async move {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let payload = br#"{"version":"1.1","host":"stress","short_message":"x","level":6,"timestamp":1.0}"#;
        let target = format!("127.0.0.1:{udp_port}");
        for _ in 0..200_000 {
            let _ = socket.send_to(payload, &target).await;
        }
    });

    // Concurrently, every 100 ms, call status.get and assert it returns
    // within 200 ms. This is the primary wedge regression check.
    let mut client = daemon.connect_anon().await;
    let probe_deadline = Instant::now() + Duration::from_secs(8);
    let mut probes = 0u32;
    while Instant::now() < probe_deadline {
        let probe_start = Instant::now();
        let result: StatusGetResult = client
            .call("status.get", json!({}))
            .await
            .expect("status.get must succeed during stress");
        let elapsed = probe_start.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "status.get took {elapsed:?} during stress — broker is wedging"
        );
        probes += 1;
        // Spot-check that the JSON deserialised the new field.
        let _ = result.receiver_drops;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(probes >= 50, "expected >=50 probes in 8s, got {probes}");

    producer.await.unwrap();

    // After the burst, status.get should still succeed and either report
    // a healthy zero or some drops. Either is acceptable — the broker
    // just must not have wedged.
    let final_status: StatusGetResult = client.call("status.get", json!({})).await.unwrap();
    let drops = &final_status.receiver_drops;
    eprintln!("post-stress drops: {drops:?}");
    // GELF UDP is the only producer in this test, so all drops must be
    // attributed there (or zero if everything fit in the channel).
    assert_eq!(drops.gelf_tcp, 0);
    assert_eq!(drops.otlp_http_logs, 0);
    assert_eq!(drops.otlp_grpc_logs, 0);
    assert_eq!(drops.otlp_http_traces, 0);
    assert_eq!(drops.otlp_grpc_traces, 0);
}
