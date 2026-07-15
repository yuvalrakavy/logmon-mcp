//! Runtime domain lifecycle (Wave 2 stage 2.2b): `domains.create` /
//! `domains.delete` / `domains.list` for EPHEMERAL domains — real port binding,
//! per-domain receivers + processors, ingest isolation, and `max_domains`.
//!
//! Durable (`config` / `persistent`) domains and their bookmark persistence are
//! a later stage; these tests only exercise ephemeral domains, which persist
//! nothing and so are independent of the persisted-state schema.
#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{
    DomainInfo, DomainsClearResult, DomainsDeleteResult, DomainsListResult,
};
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
    let payload = format!(r#"{{"version":"1.1","host":"test","short_message":"{msg}","level":6}}"#);
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

/// A stateless dev-track re-creates its domain on every startup with the SAME
/// explicit (derived) ports — `domains.create{name, gelf_port, otlp_*}` repeated
/// with identical params must be an idempotent no-op returning the existing
/// domain, NOT a conflict error. (Omitted-ports idempotency is covered above;
/// dev-tracks pass derived ports, so this locks in the explicit-ports path.)
#[tokio::test]
async fn create_with_same_explicit_ports_is_idempotent() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // First create — capture the allocated ports (stand-ins for a dev-track's
    // derived ports).
    let first: DomainInfo = client
        .call("domains.create", json!({ "name": "track12" }))
        .await
        .expect("first create");

    // Re-create with the SAME explicit ports → idempotent success.
    let again: DomainInfo = client
        .call(
            "domains.create",
            json!({
                "name": "track12",
                "gelf_port": first.gelf_port,
                "otlp_grpc_port": first.otlp_grpc_port,
                "otlp_http_port": first.otlp_http_port,
            }),
        )
        .await
        .expect("re-create with identical explicit ports must succeed (idempotent)");
    assert_eq!(again.gelf_port, first.gelf_port);
    assert_eq!(again.otlp_grpc_port, first.otlp_grpc_port);
    assert_eq!(again.otlp_http_port, first.otlp_http_port);
    assert_eq!(again.source, "ephemeral");

    // And still exactly one domain — the re-ensure must not duplicate it.
    let list: DomainsListResult = client.call("domains.list", json!({})).await.unwrap();
    assert_eq!(
        list.domains.iter().filter(|d| d.name == "track12").count(),
        1,
        "re-ensure must not duplicate the domain"
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
    assert_eq!(
        wait_log_count(&mut client, "t3", 1).await,
        1,
        "landed in t3"
    );

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
    assert!(
        cleared.logs_cleared >= 1,
        "at least the one log was cleared"
    );

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

/// `domains.create` rejects an oversized `log_buffer_size` at the RPC boundary
/// instead of accepting it and later ABORTING the whole process when the ring is
/// lazily reserved on first ingest (`VecDeque::reserve_exact` is infallible → an
/// allocation failure invokes the global alloc-error handler, which aborts —
/// killing every domain and disconnecting every client, not just the offender).
#[tokio::test]
async fn create_rejects_oversized_log_buffer() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // ~1e11 entries: astronomically beyond any real ring; must be refused, never
    // realized. (We deliberately do NOT ingest — pre-fix that would abort the
    // test process itself.)
    let res: anyhow::Result<DomainInfo> = client
        .call(
            "domains.create",
            json!({ "name": "huge", "log_buffer_size": 100_000_000_000u64 }),
        )
        .await;
    assert!(
        res.is_err(),
        "oversized log_buffer_size must be rejected, got {res:?}"
    );
    assert!(
        get_domain(&mut client, "huge").await.is_none(),
        "a rejected create must leave no domain behind"
    );
}

/// Concurrent `domains.create` for the SAME name must converge to a single live
/// domain (idempotent ensure). Pre-fix, the existence-check → port-bind →
/// registry-insert straddles the bind `.await` on the shared handler, so N racers
/// each pass the `get()==None` check, each bind their OWN ports, and the last
/// `insert` silently overwrites (and tears down) the others — every caller but
/// one is handed a port that is closed moments later. All racers must instead
/// observe the same live domain (same allocated ports).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_same_name_converges_to_one_domain() {
    let daemon = spawn_test_daemon().await;

    // Establish all connections FIRST (sequentially). The race under test is
    // concurrent CREATE, not concurrent connect — opening 8 clients at once
    // stresses the accept loop and flakes ("connection refused") under full-suite
    // load, which is incidental to what this test asserts.
    let n = 8;
    let mut clients = Vec::new();
    for _ in 0..n {
        clients.push(TestClient::connect(&daemon.socket_path, None, None).await);
    }
    let mut handles = Vec::new();
    for mut c in clients {
        handles.push(tokio::spawn(async move {
            c.call::<DomainInfo>("domains.create", json!({ "name": "race" }))
                .await
        }));
    }

    let mut ports = Vec::new();
    for h in handles {
        let info = h
            .await
            .unwrap()
            .expect("each concurrent create should succeed idempotently");
        ports.push(info.gelf_port);
    }

    let first = ports[0];
    assert!(
        ports.iter().all(|&p| p == first),
        "all concurrent same-name creates must converge to one live domain; got gelf_ports {ports:?}"
    );

    let mut client = daemon.connect_anon().await;
    let list: DomainsListResult = client.call("domains.list", json!({})).await.unwrap();
    let count = list.domains.iter().filter(|d| d.name == "race").count();
    assert_eq!(
        count, 1,
        "exactly one 'race' domain must exist, got {count}"
    );
}

/// The same create-serialization must also prevent `max_domains` OVERSHOOT:
/// N racers with distinct names near the cap must not all pass the pre-insert
/// count check. Exactly `max_domains` may succeed; the rest get the cap error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_respects_max_domains() {
    let mut config = default_test_config();
    config.max_domains = 2; // `default` is Config-sourced and doesn't count.
    let daemon = TestDaemonHandle::spawn_with_config(config).await;

    // Connect first (see `concurrent_create_same_name`): isolate the concurrent
    // CREATE race from concurrent-connect flakiness under full-suite load.
    let n = 8;
    let mut clients = Vec::new();
    for _ in 0..n {
        clients.push(TestClient::connect(&daemon.socket_path, None, None).await);
    }
    let mut handles = Vec::new();
    for (i, mut c) in clients.into_iter().enumerate() {
        handles.push(tokio::spawn(async move {
            c.call::<DomainInfo>("domains.create", json!({ "name": format!("d{i}") }))
                .await
        }));
    }

    let mut ok = 0;
    for h in handles {
        if h.await.unwrap().is_ok() {
            ok += 1;
        }
    }
    assert_eq!(
        ok, 2,
        "exactly max_domains (2) creates may succeed, got {ok}"
    );

    let mut client = daemon.connect_anon().await;
    let list: DomainsListResult = client.call("domains.list", json!({})).await.unwrap();
    let api = list.domains.iter().filter(|d| d.name != "default").count();
    assert_eq!(
        api, 2,
        "registry must hold exactly max_domains API domains, got {api}"
    );
}

/// Same abort-prevention guard for `span_buffer_size`.
#[tokio::test]
async fn create_rejects_oversized_span_buffer() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let res: anyhow::Result<DomainInfo> = client
        .call(
            "domains.create",
            json!({ "name": "huge", "span_buffer_size": 100_000_000_000u64 }),
        )
        .await;
    assert!(
        res.is_err(),
        "oversized span_buffer_size must be rejected, got {res:?}"
    );
    assert!(
        get_domain(&mut client, "huge").await.is_none(),
        "a rejected create must leave no domain behind"
    );
}

/// Consumer #2: liveness — `last_log_received_at` + `idle_secs` populate on
/// ingest; a never-received domain reports `None` (the "nothing is shipping to
/// this domain — misconfigured port?" signal) and is not stale.
#[tokio::test]
async fn domain_liveness_tracks_last_received_and_idle() {
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

    // Fresh domains — nothing received yet.
    assert!(t3.last_log_received_at.is_none() && t3.idle_secs.is_none() && !t3.stale);
    assert!(t4.last_log_received_at.is_none());

    // Ingest a GELF log into t3.
    send_gelf(t3.gelf_port, "hello").await;
    assert_eq!(wait_log_count(&mut client, "t3", 1).await, 1);

    let t3 = get_domain(&mut client, "t3").await.unwrap();
    assert!(
        t3.last_log_received_at.is_some(),
        "t3 received a log → last_log_received_at set"
    );
    assert!(
        t3.idle_secs.is_some_and(|i| i < 10),
        "just received → small idle: {:?}",
        t3.idle_secs
    );
    assert!(!t3.stale, "fresh (default 60s threshold) → not stale");

    // t4 still never received → the misconfigured-port signal.
    let t4 = get_domain(&mut client, "t4").await.unwrap();
    assert!(
        t4.last_log_received_at.is_none() && t4.idle_secs.is_none() && !t4.stale,
        "no ingest → None + not stale"
    );
}

/// Consumer #2: the `stale` flag honors the configurable `stale_after_secs`;
/// a never-received domain is never stale.
#[tokio::test]
async fn domain_stale_flag_honors_threshold() {
    let mut config = default_test_config();
    config.stale_after_secs = 0; // any received domain is immediately "stale"
    let daemon = TestDaemonHandle::spawn_with_config(config).await;
    let mut client = daemon.connect_anon().await;
    let t3: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();

    send_gelf(t3.gelf_port, "x").await;
    assert_eq!(wait_log_count(&mut client, "t3", 1).await, 1);
    let t3 = get_domain(&mut client, "t3").await.unwrap();
    assert!(
        t3.stale,
        "threshold 0 + received → stale (idle {:?})",
        t3.idle_secs
    );

    // A never-received domain is NOT stale even at threshold 0.
    let t4: DomainInfo = client
        .call("domains.create", json!({ "name": "t4" }))
        .await
        .unwrap();
    assert!(!t4.stale, "never received → not stale even at threshold 0");
}

/// Consumer #4: a domain's OTLP gRPC and HTTP arms can't share a port — caught
/// with a clear error, not the cryptic bind failure. (Both 0 = disabled is OK.)
#[tokio::test]
async fn create_rejects_equal_otlp_ports() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let res: anyhow::Result<DomainInfo> = client
        .call(
            "domains.create",
            json!({ "name": "x", "otlp_grpc_port": 5555, "otlp_http_port": 5555 }),
        )
        .await;
    let err = res
        .expect_err("equal OTLP ports must be rejected")
        .to_string();
    assert!(
        err.contains("must differ"),
        "expected a clear 'must differ' error, got: {err}"
    );

    // GELF sharing an OTLP port is also caught (generalized to all port pairs).
    let res: anyhow::Result<DomainInfo> = client
        .call(
            "domains.create",
            json!({ "name": "z", "gelf_port": 6666, "otlp_grpc_port": 6666, "otlp_http_port": 0 }),
        )
        .await;
    assert!(
        res.expect_err("gelf == otlp_grpc must be rejected")
            .to_string()
            .contains("must differ"),
        "any two coinciding non-zero ports must be rejected"
    );

    // Both 0 (disabled) is not a collision.
    let ok: DomainInfo = client
        .call(
            "domains.create",
            json!({ "name": "y", "otlp_grpc_port": 0, "otlp_http_port": 0 }),
        )
        .await
        .expect("both-disabled OTLP is fine");
    assert_eq!(ok.name, "y");
}
