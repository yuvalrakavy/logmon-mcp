//! The OTEL availability beacons. A daemon announces `OFFLINE` only if it announced
//! `ONLINE` — the beacon carries no port, so every tracing-init producer on the host opens
//! its circuit breaker on it — and no TEST daemon ever reaches the host: the harness points
//! `DaemonOverrides::beacon_target` at a socket of the test's own, where these tests read
//! what each daemon announced.
#![cfg(feature = "test-support")]

use logmon_broker_core::daemon::persistence::DaemonConfig;
use logmon_broker_core::daemon::server::{run_with_overrides, DaemonOverrides};
use logmon_broker_core::test_support::*;

/// Two DISTINCT free ports (gRPC, HTTP): both listeners are held until both ports are chosen,
/// so the kernel cannot hand back the same one twice. (Another process taking one before the
/// daemon binds it is still possible; the vacuity guard below turns that into a red run rather
/// than a silent pass.)
fn free_ports() -> (u16, u16) {
    let a = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let b = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    (
        a.local_addr().expect("bound").port(),
        b.local_addr().expect("bound").port(),
    )
}

/// A daemon whose OTLP receiver started but whose startup then failed — here the socket bind,
/// into a directory that does not exist — announces nothing. It used to send `ONLINE` as soon
/// as the receiver started and then exit through `?` without `OFFLINE`, leaving every producer
/// on the host pointed at a collector that was never there (gh #27).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[tracing_test::traced_test]
async fn a_daemon_that_fails_after_its_receivers_start_announces_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let beacons = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    beacons.set_nonblocking(true).unwrap();
    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (grpc, http) = free_ports();
    let config = DaemonConfig {
        gelf_port: 0,
        otlp_grpc_port: grpc,
        otlp_http_port: http,
        ..DaemonConfig::default()
    };
    let result = run_with_overrides(
        config,
        DaemonOverrides {
            config_dir: Some(dir.path().to_path_buf()),
            socket_path: Some(dir.path().join("no-such-dir").join("logmon.sock")),
            injected_log_rx: None,
            shutdown_rx: Some(shutdown_rx),
            accept_paused: None,
            skip_tracing_init: true,
            beacon_target: beacons.local_addr().ok(),
        },
    )
    .await;
    assert!(result.is_err(), "the socket bind must fail: {result:?}");
    // Not vacuous: the OTLP receiver must actually have started. A port lost to the
    // bind-then-drop race in `free_ports` degrades OTLP to disabled (a WARN, and startup goes
    // on), and a broker with no OTLP receiver announces nothing whether or not it should.
    assert!(
        logs_contain("OTLP receiver started"),
        "the OTLP receiver never started, so this run proves nothing"
    );
    let mut buf = [0u8; 64];
    let mut seen = Vec::new();
    while let Ok((n, _)) = beacons.recv_from(&mut buf) {
        seen.push(String::from_utf8_lossy(&buf[..n]).into_owned());
    }
    assert_eq!(seen, Vec::<String>::new());
}

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
    let (grpc, http) = free_ports();
    let config = DaemonConfig {
        gelf_port: 0,
        otlp_grpc_port: grpc,
        otlp_http_port: http,
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
