//! Integration tests for the SDK's typed `domains.*` dispatch (Wave 2 stage
//! 2.4). Exercises the full ephemeral-domain lifecycle through the typed Broker
//! API — thin wrappers over the already-gated wire methods, verified end-to-end.

use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;
use logmon_broker_protocol::{DomainsClear, DomainsCreate, DomainsDelete, DomainsList, DomainsUse};
use logmon_broker_sdk::Broker;

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

    // use — binds the session; returns the bound domain.
    let bound = broker
        .domains_use(DomainsUse { name: "t3".into() })
        .await
        .unwrap();
    assert_eq!(bound.name, "t3");

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
