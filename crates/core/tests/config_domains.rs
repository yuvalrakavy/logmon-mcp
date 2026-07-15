//! Config-declared domains (§17.9): `config.json` `domains: [...]` re-created at
//! boot, DECLARATIONS-ONLY (empty buffers, fresh seq — data never persists).
//! Isolated like any domain; `domains.delete` refuses them; a `default`-named or
//! port-clashing entry is skipped with the daemon staying up.
#![cfg(feature = "test-support")]

use logmon_broker_core::daemon::persistence::ConfigDomain;
use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{DomainInfo, DomainsListResult, LogsRecentResult};
use serde_json::json;
use std::time::Duration;
use tokio::net::UdpSocket;

fn cfg_domain(name: &str) -> ConfigDomain {
    ConfigDomain {
        name: name.into(),
        gelf_port: None,         // auto-allocate
        otlp_grpc_port: Some(0), // OTLP disabled for these tests
        otlp_http_port: Some(0),
        log_buffer_size: None,
        span_buffer_size: None,
    }
}

async fn get_domain(client: &mut TestClient, name: &str) -> Option<DomainInfo> {
    let list: DomainsListResult = client.call("domains.list", json!({})).await.unwrap();
    list.domains.into_iter().find(|d| d.name == name)
}

async fn count_domain(client: &mut TestClient, name: &str) -> usize {
    let list: DomainsListResult = client.call("domains.list", json!({})).await.unwrap();
    list.domains.iter().filter(|d| d.name == name).count()
}

async fn send_gelf(port: u16, msg: &str) {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let payload =
        format!(r#"{{"version":"1.1","host":"test","short_message":"{msg}","level":6}}"#);
    sock.send_to(payload.as_bytes(), format!("127.0.0.1:{port}"))
        .await
        .unwrap();
}

#[tokio::test]
async fn config_domain_boots_and_ingests_isolated() {
    let mut config = default_test_config();
    config.domains = vec![cfg_domain("staging")];
    let daemon = TestDaemonHandle::spawn_with_config(config).await;
    let mut client = daemon.connect_anon().await;

    let staging = get_domain(&mut client, "staging")
        .await
        .expect("staging config domain must be present at boot");
    assert_eq!(staging.source, "config");
    assert_ne!(staging.gelf_port, 0, "staging got an allocated GELF port");

    // Ingest into staging → lands in staging.
    send_gelf(staging.gelf_port, "staging log").await;
    client
        .call::<DomainInfo>("domains.use", json!({ "name": "staging" }))
        .await
        .unwrap();
    let mut got = false;
    for _ in 0..40 {
        let r: LogsRecentResult = client.call("logs.recent", json!({ "count": 50 })).await.unwrap();
        if r.logs.iter().any(|l| l.message.contains("staging log")) {
            got = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(got, "staging log should ingest into the staging domain");

    // default is untouched.
    client
        .call::<DomainInfo>("domains.use", json!({ "name": "default" }))
        .await
        .unwrap();
    let r: LogsRecentResult = client.call("logs.recent", json!({ "count": 50 })).await.unwrap();
    assert!(
        !r.logs.iter().any(|l| l.message.contains("staging log")),
        "staging log must not leak into default"
    );
}

#[tokio::test]
async fn config_domain_named_default_is_skipped() {
    let mut config = default_test_config();
    // A `default`-named entry must be rejected (not overwrite the real default);
    // a valid sibling proves config domains are actually being built.
    config.domains = vec![cfg_domain("default"), cfg_domain("ok")];
    let daemon = TestDaemonHandle::spawn_with_config(config).await;
    let mut client = daemon.connect_anon().await;

    assert_eq!(
        count_domain(&mut client, "default").await,
        1,
        "the config 'default' entry must be skipped, not create a duplicate/overwrite"
    );
    assert!(
        get_domain(&mut client, "ok").await.is_some(),
        "a valid sibling config domain must still boot"
    );
}

#[tokio::test]
async fn config_domain_port_clash_is_skipped_daemon_stays_up() {
    // Hold a UDP port so the clashing config domain's GELF bind fails.
    let squatter = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let taken = squatter.local_addr().unwrap().port();

    let mut config = default_test_config();
    let mut clash = cfg_domain("clash");
    clash.gelf_port = Some(taken); // clashes with the squatter
    config.domains = vec![clash, cfg_domain("ok")];
    let daemon = TestDaemonHandle::spawn_with_config(config).await;
    let mut client = daemon.connect_anon().await;

    // Daemon booted; the clashing domain is absent, the sibling is present.
    assert!(
        get_domain(&mut client, "clash").await.is_none(),
        "a port-clashing config domain must be skipped"
    );
    assert!(
        get_domain(&mut client, "ok").await.is_some(),
        "a non-clashing sibling must still boot (proves config domains build)"
    );
    drop(squatter);
}

/// Zero-migration: a `config.json` written before `domains` existed must load
/// with `domains == []` (the field is `#[serde(default)]`).
#[test]
fn old_config_without_domains_loads_empty() {
    let json = r#"{"gelf_port":12201,"buffer_size":10000,"otlp_grpc_port":4317}"#;
    let cfg: logmon_broker_core::daemon::persistence::DaemonConfig =
        serde_json::from_str(json).unwrap();
    assert!(
        cfg.domains.is_empty(),
        "an old config.json without `domains` must load with no config domains"
    );
}

#[tokio::test]
async fn config_domain_delete_is_refused() {
    let mut config = default_test_config();
    config.domains = vec![cfg_domain("staging")];
    let daemon = TestDaemonHandle::spawn_with_config(config).await;
    let mut client = daemon.connect_anon().await;

    assert!(get_domain(&mut client, "staging").await.is_some());
    let deleted = client
        .call::<serde_json::Value>("domains.delete", json!({ "name": "staging" }))
        .await;
    assert!(deleted.is_err(), "deleting a config domain must be refused");
    assert!(
        get_domain(&mut client, "staging").await.is_some(),
        "the refused config domain must still be present"
    );
}
