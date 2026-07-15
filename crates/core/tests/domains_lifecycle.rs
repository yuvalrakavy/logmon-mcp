//! Runtime domain lifecycle (Wave 2 stage 2.2b): `domains.create` /
//! `domains.delete` / `domains.list` for EPHEMERAL domains — real port binding,
//! per-domain receivers + processors, ingest isolation, and `max_domains`.
//!
//! Durable (`config` / `persistent`) domains and their bookmark persistence are
//! a later stage; these tests only exercise ephemeral domains, which persist
//! nothing and so are independent of the persisted-state schema.
#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{DomainInfo, DomainsClearResult, DomainsDeleteResult, DomainsListResult};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

// --- helpers ---------------------------------------------------------------

/// Fetch a domain's info from `domains.list` by name.
async fn get_domain(client: &mut TestClient, name: &str) -> Option<DomainInfo> {
    let list: DomainsListResult = client.call("domains.list", json!({})).await.unwrap();
    list.domains.into_iter().find(|d| d.name == name)
}

/// Poll `domains.list` until `name`'s `log_count` reaches `want`, or ~2s.
async fn wait_log_count(client: &mut TestClient, name: &str, want: usize) -> usize {
    for _ in 0..40 {
        if let Some(d) = get_domain(client, name).await {
            if d.log_count >= want {
                return d.log_count;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    get_domain(client, name).await.map_or(0, |d| d.log_count)
}

/// Send one well-formed GELF/UDP datagram to `127.0.0.1:port`.
async fn send_gelf(port: u16, msg: &str) {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let payload =
        format!(r#"{{"version":"1.1","host":"test","short_message":"{msg}","level":6}}"#);
    sock.send_to(payload.as_bytes(), format!("127.0.0.1:{port}"))
        .await
        .unwrap();
}

/// POST a minimal OTLP/HTTP logs payload to `127.0.0.1:port/v1/logs`; return the
/// HTTP status code (0 on failure). Raw HTTP/1.1 to avoid a client dependency.
async fn post_otlp_log(http_port: u16, msg: &str) -> u16 {
    let body = format!(
        r#"{{"resourceLogs":[{{"resource":{{"attributes":[]}},"scopeLogs":[{{"logRecords":[{{"body":{{"stringValue":"{msg}"}},"severityNumber":9}}]}}]}}]}}"#
    );
    let req = format!(
        "POST /v1/logs HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let mut stream = TcpStream::connect(format!("127.0.0.1:{http_port}"))
        .await
        .unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let resp = tokio::time::timeout(Duration::from_secs(2), async {
        let mut s = String::new();
        let _ = stream.read_to_string(&mut s).await;
        s
    })
    .await
    .unwrap_or_default();
    resp.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

// --- tests -----------------------------------------------------------------

#[tokio::test]
async fn create_returns_ephemeral_domain_and_appears_in_list() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let created: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .expect("domains.create should succeed");
    assert_eq!(created.name, "t3");
    assert_eq!(created.source, "ephemeral");

    let list: DomainsListResult = client
        .call("domains.list", json!({}))
        .await
        .expect("domains.list should succeed");
    let names: Vec<&str> = list.domains.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"default"), "default present: {names:?}");
    assert!(names.contains(&"t3"), "t3 present: {names:?}");
}

#[tokio::test]
async fn delete_removes_ephemeral_domain() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    client
        .call::<DomainInfo>("domains.create", json!({ "name": "t3" }))
        .await
        .expect("create t3");

    let deleted: DomainsDeleteResult = client
        .call("domains.delete", json!({ "name": "t3" }))
        .await
        .expect("delete t3");
    assert_eq!(deleted.name, "t3");
    assert!(deleted.deleted);

    let list: DomainsListResult = client.call("domains.list", json!({})).await.unwrap();
    let names: Vec<&str> = list.domains.iter().map(|d| d.name.as_str()).collect();
    assert!(!names.contains(&"t3"), "t3 gone after delete: {names:?}");
    assert!(names.contains(&"default"), "default remains: {names:?}");
}

#[tokio::test]
async fn delete_refuses_default_domain() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let result = client
        .call::<DomainsDeleteResult>("domains.delete", json!({ "name": "default" }))
        .await;
    assert!(
        result.is_err(),
        "deleting the config-declared default domain must be refused"
    );
}

#[tokio::test]
async fn create_is_idempotent_and_rejects_conflicting_ports() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let first: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();
    let gelf = first.gelf_port;

    // Re-create with omitted ports → idempotent no-op returning the same ports.
    let again: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();
    assert_eq!(again.gelf_port, gelf, "re-create is a no-op, same ports");

    // Re-create with a DIFFERENT explicit gelf_port → conflict error.
    let conflict = client
        .call::<DomainInfo>(
            "domains.create",
            json!({ "name": "t3", "gelf_port": gelf.wrapping_add(1) }),
        )
        .await;
    assert!(
        conflict.is_err(),
        "conflicting ports on an existing domain must error"
    );
}

#[tokio::test]
async fn create_rejects_otlp_port_clash_across_domains() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let t3: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();
    let grpc = t3.otlp_grpc_port;
    assert_ne!(grpc, 0);

    // t4 asking for t3's OTLP gRPC port must fail with a clean synchronous error
    // (the OTLP pre-bind, stage 2.2a) — not a half-created domain.
    let clash = client
        .call::<DomainInfo>(
            "domains.create",
            json!({ "name": "t4", "otlp_grpc_port": grpc }),
        )
        .await;
    assert!(
        clash.is_err(),
        "a cross-domain OTLP port clash must be a clean error"
    );
    assert!(
        get_domain(&mut client, "t4").await.is_none(),
        "a failed create leaves no domain behind"
    );
}

#[tokio::test]
async fn create_then_gelf_ingest_is_isolated_to_the_domain() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let t3: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();
    assert_ne!(t3.gelf_port, 0, "t3 has a real GELF port");

    // A datagram to t3's port lands in t3's pipeline — and ONLY t3's.
    send_gelf(t3.gelf_port, "hello t3").await;
    assert_eq!(wait_log_count(&mut client, "t3", 1).await, 1, "landed in t3");

    let default_count = get_domain(&mut client, "default").await.unwrap().log_count;
    assert_eq!(
        default_count, 0,
        "default received nothing — per-domain receiver isolation"
    );
}

#[tokio::test]
async fn create_then_otlp_http_ingest_lands_in_the_domain() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let t3: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();
    assert_ne!(t3.otlp_http_port, 0, "t3 has a real OTLP HTTP port");

    let status = post_otlp_log(t3.otlp_http_port, "otlp hello").await;
    assert_eq!(status, 200, "OTLP/HTTP export returns 200");

    assert_eq!(
        wait_log_count(&mut client, "t3", 1).await,
        1,
        "the OTLP log landed in t3's pipeline (closes the 2.2a serve loop)"
    );
}

#[tokio::test]
async fn max_domains_refuses_beyond_the_cap() {
    let mut config = default_test_config();
    config.max_domains = 1;
    let daemon = TestDaemonHandle::spawn_with_config(config).await;
    let mut client = daemon.connect_anon().await;

    client
        .call::<DomainInfo>("domains.create", json!({ "name": "t1" }))
        .await
        .expect("first API-created domain is allowed");
    let refused = client
        .call::<DomainInfo>("domains.create", json!({ "name": "t2" }))
        .await;
    assert!(
        refused.is_err(),
        "max_domains=1 refuses a second API-created domain (config/default don't count)"
    );
}

#[tokio::test]
async fn port_zero_disables_gelf_but_keeps_otlp() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let t3: DomainInfo = client
        .call("domains.create", json!({ "name": "t3", "gelf_port": 0 }))
        .await
        .unwrap();
    assert_eq!(t3.gelf_port, 0, "gelf_port=0 disables GELF");
    assert_ne!(t3.otlp_grpc_port, 0, "OTLP is still allocated");
    assert_ne!(t3.otlp_http_port, 0, "OTLP HTTP is still allocated");
}

#[tokio::test]
async fn delete_releases_ports_for_reuse() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let t3: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();
    let (gelf, grpc, http) = (t3.gelf_port, t3.otlp_grpc_port, t3.otlp_http_port);

    client
        .call::<DomainsDeleteResult>("domains.delete", json!({ "name": "t3" }))
        .await
        .unwrap();

    // The freed ports become reusable once the receivers stop (Arc-graceful
    // teardown via DomainReceivers::drop). Release is async — the listener
    // tasks process their shutdown signal — so poll.
    let mut reused = false;
    for _ in 0..40 {
        let r = client
            .call::<DomainInfo>(
                "domains.create",
                json!({
                    "name": "t4",
                    "gelf_port": gelf,
                    "otlp_grpc_port": grpc,
                    "otlp_http_port": http,
                }),
            )
            .await;
        if r.is_ok() {
            reused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        reused,
        "deleted domain's ports (gelf={gelf}, grpc={grpc}, http={http}) must be reusable after teardown"
    );
}

#[tokio::test]
async fn clear_disposes_bound_domain_data_and_keeps_seq_monotonic() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let t3: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();
    client
        .call::<DomainInfo>("domains.use", json!({ "name": "t3" }))
        .await
        .unwrap();

    send_gelf(t3.gelf_port, "before clear").await;
    assert_eq!(wait_log_count(&mut client, "t3", 1).await, 1);
    let newest_before = get_domain(&mut client, "t3")
        .await
        .unwrap()
        .newest_seq
        .expect("t3 has a newest seq after ingest");

    // Clear the bound domain's data.
    let cleared: DomainsClearResult = client
        .call("domains.clear", json!({}))
        .await
        .expect("domains.clear");
    assert!(cleared.logs_cleared >= 1, "at least the one log was cleared");

    let after = get_domain(&mut client, "t3").await.unwrap();
    assert_eq!(after.log_count, 0, "logs disposed");
    assert_eq!(after.span_count, 0, "spans disposed");

    // A record after clear gets a seq ABOVE the pre-clear newest — seq stays
    // monotonic (clear disposes data, not the counter).
    send_gelf(t3.gelf_port, "after clear").await;
    assert_eq!(wait_log_count(&mut client, "t3", 1).await, 1);
    let newest_after = get_domain(&mut client, "t3")
        .await
        .unwrap()
        .newest_seq
        .expect("t3 has a newest seq after re-ingest");
    assert!(
        newest_after > newest_before,
        "seq stays monotonic across clear: {newest_after} > {newest_before}"
    );
}

/// `domains.clear` clears ONLY the bound domain — a sibling domain is untouched.
#[tokio::test]
async fn clear_is_scoped_to_the_bound_domain() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let t3: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();
    let t4: DomainInfo = client
        .call("domains.create", json!({ "name": "t4" }))
        .await
        .unwrap();

    send_gelf(t3.gelf_port, "t3 data").await;
    send_gelf(t4.gelf_port, "t4 data").await;

    // Bind to t3 and clear.
    client
        .call::<DomainInfo>("domains.use", json!({ "name": "t3" }))
        .await
        .unwrap();
    assert_eq!(wait_log_count(&mut client, "t3", 1).await, 1);
    let _: DomainsClearResult = client.call("domains.clear", json!({})).await.unwrap();

    assert_eq!(
        get_domain(&mut client, "t3").await.unwrap().log_count,
        0,
        "t3 (bound) was cleared"
    );
    // t4's data survives.
    assert_eq!(
        wait_log_count(&mut client, "t4", 1).await,
        1,
        "t4 (unbound) is untouched by clear"
    );
}
