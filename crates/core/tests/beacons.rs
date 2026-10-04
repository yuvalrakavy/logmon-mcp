//! The OTEL availability beacons. A daemon announces `OFFLINE` only if it announced
//! `ONLINE` — the beacon carries no port, so every tracing-init producer on the host opens
//! its circuit breaker on it — and no TEST daemon ever reaches the host: the harness points
//! `DaemonOverrides::beacon_target` at a socket of the test's own, where these tests read
//! what each daemon announced.
#![cfg(feature = "test-support")]

use logmon_broker_core::daemon::persistence::DaemonConfig;
use logmon_broker_core::test_support::*;

/// A daemon on an injected channel starts no OTLP receiver, announces nothing, and so owes
/// no `OFFLINE` when it stops. It used to send one anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_never_announced_itself_announces_nothing_on_shutdown() {
    let d = TestDaemonHandle::spawn().await;
    d.shutdown().await;
    assert_eq!(d.beacons_received(), Vec::<String>::new());
}

/// A daemon whose OTLP receiver started announces `ONLINE`, and `OFFLINE` when it stops —
/// to the test's own socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_with_otlp_announces_online_then_offline() {
    let free = || {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        l.local_addr().expect("bound").port()
    };
    let config = DaemonConfig {
        gelf_port: 0,
        otlp_grpc_port: free(),
        otlp_http_port: free(),
        ..DaemonConfig::default()
    };
    let d = TestDaemonHandle::spawn_with_real_receivers_config(config).await;
    let mut seen = d.beacons_received();
    d.shutdown().await;
    seen.extend(d.beacons_received());
    assert_eq!(
        seen,
        vec!["OTEL:ONLINE\n".to_string(), "OTEL:OFFLINE\n".to_string()]
    );
}
