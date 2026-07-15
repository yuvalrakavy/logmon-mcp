//! Boot-time resilience (Wave 2 deep-gate finding C). An OPTIONAL OTLP receiver
//! whose port is already taken must NOT take down the whole daemon (and with it
//! GELF/logging). GELF remains the core function and still fails loud on a clash;
//! OTLP is optional and degrades loudly. Explicit `domains.create` keeps its
//! fail-loud contract (that path propagates the bind error to the caller).
#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::StatusGetResult;
use serde_json::json;

/// Occupy the OTLP gRPC port, then boot a daemon configured to use it. The daemon
/// must still come up — GELF listening, control socket serving — with OTLP
/// disabled, rather than aborting boot. Pre-fix, `OtlpReceiver::start(...).await?`
/// propagates the bind error out of `run_daemon` and the process exits before the
/// control socket is ever created (so `spawn_with_real_receivers_config` panics on
/// "socket never appeared").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn otlp_grpc_port_clash_at_boot_degrades_not_fatal() {
    // Hold a listener on an ephemeral port for the whole test so the daemon's
    // gRPC bind to the same port clashes.
    let squatter = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let taken = squatter.local_addr().unwrap().port();

    let mut config = default_test_config();
    config.otlp_grpc_port = taken; // clashes with the squatter
    config.otlp_http_port = 0; // http disabled → only the gRPC clash matters

    let daemon = TestDaemonHandle::spawn_with_real_receivers_config(config).await;

    // The daemon booted and is serving. GELF must still be listening.
    let mut client = daemon.connect_anon().await;
    let status: StatusGetResult = client.call("status.get", json!({})).await.unwrap();
    assert!(
        status
            .receivers
            .iter()
            .any(|r| r.starts_with("UDP:") || r.starts_with("TCP:")),
        "GELF must still be listening after an OTLP boot clash: {:?}",
        status.receivers
    );

    drop(squatter);
}
