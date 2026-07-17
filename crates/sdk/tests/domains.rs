//! Integration tests for the SDK's typed `domains.*` dispatch (Wave 2 stage
//! 2.4). Exercises the full ephemeral-domain lifecycle through the typed Broker
//! API — thin wrappers over the already-gated wire methods, verified end-to-end.

use logmon_broker_core::daemon::persistence::ConfigDomain;
use logmon_broker_core::test_support::{
    default_test_config, spawn_test_daemon_for_sdk, TestDaemonHandle,
};
use logmon_broker_protocol::{
    DomainsClear, DomainsCreate, DomainsDelete, DomainsList, DomainsUse, StatusGet, StatusGetResult,
};
use logmon_broker_sdk::Broker;

fn cfg_domain(name: &str) -> ConfigDomain {
    ConfigDomain {
        name: name.into(),
        gelf_port: Some(0),
        otlp_grpc_port: Some(0),
        otlp_http_port: Some(0),
        log_buffer_size: None,
        span_buffer_size: None,
    }
}

#[tokio::test]
async fn domains_typed_lifecycle_round_trip() {
    let daemon = spawn_test_daemon_for_sdk().await;
    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .open()
        .await
        .unwrap();

    // create (idempotent ensure) with auto-allocated ports.
    let created = broker
        .domains_create(DomainsCreate {
            name: "t3".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(created.name, "t3");
    assert_eq!(created.source, "ephemeral");

    // list — default + t3.
    let list = broker.domains_list(DomainsList {}).await.unwrap();
    let names: Vec<&str> = list.domains.iter().map(|d| d.name.as_str()).collect();
    assert!(
        names.contains(&"default") && names.contains(&"t3"),
        "got {names:?}"
    );

    // use — binds the session; returns the bound domain. First bind from
    // `default` is the normal lifecycle: no rebind warning.
    let bound = broker
        .domains_use(DomainsUse { name: "t3".into() })
        .await
        .unwrap();
    assert_eq!(bound.domain.name, "t3");
    assert!(bound.rebind_warning.is_none());

    // clear — disposes the bound domain's data (empty here → 0/0).
    let cleared = broker.domains_clear(DomainsClear {}).await.unwrap();
    assert_eq!(cleared.logs_cleared, 0);
    assert_eq!(cleared.spans_cleared, 0);

    // delete.
    let deleted = broker
        .domains_delete(DomainsDelete { name: "t3".into() })
        .await
        .unwrap();
    assert!(deleted.deleted);

    // gone from list.
    let list = broker.domains_list(DomainsList {}).await.unwrap();
    assert!(
        !list.domains.iter().any(|d| d.name == "t3"),
        "t3 should be gone after delete"
    );
}

/// #1: `Broker::connect().domain(name)` binds the session at the session.start
/// handshake — verified via status.get.current_domain.
#[tokio::test]
async fn connect_domain_binds_at_handshake() {
    let mut config = default_test_config();
    config.domains = vec![cfg_domain("t1")];
    let daemon = TestDaemonHandle::spawn_with_config(config).await;
    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .domain("t1")
        .open()
        .await
        .unwrap();
    let status: StatusGetResult = broker.status_get(StatusGet {}).await.unwrap();
    assert_eq!(status.current_domain, "t1", "connect-time domain bind");
}

/// #1 load-bearing: the connect-time domain bind must be RE-SENT on reconnect,
/// so a daemon restart re-binds the same domain instead of silently reverting to
/// `default` (the exact silent-wrong-track failure this feature prevents). Uses a
/// CONFIG domain so it survives the restart.
#[tokio::test]
async fn reconnect_preserves_bound_domain() {
    let mut config = default_test_config();
    config.domains = vec![cfg_domain("t1")];
    let mut daemon = TestDaemonHandle::spawn_with_config(config).await;
    // A NAMED session (the reconnect-resilient mode) — anonymous sessions can't
    // resume across a restart. This is what a restart-resilient MCP shim uses.
    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .session_name("track")
        .domain("t1")
        .open()
        .await
        .unwrap();
    assert_eq!(
        broker
            .status_get(StatusGet {})
            .await
            .unwrap()
            .current_domain,
        "t1"
    );

    // Restart the daemon; the config domain t1 is re-created at boot.
    daemon.restart().await;

    // The next call drives reconnect — the domain must be re-bound to t1.
    let status = broker.status_get(StatusGet {}).await.unwrap();
    assert_eq!(
        status.current_domain, "t1",
        "reconnect must re-send the domain and re-bind t1, not revert to default"
    );
}
