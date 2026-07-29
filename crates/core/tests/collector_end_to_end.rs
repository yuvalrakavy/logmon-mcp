//! The driving workflow, over a real daemon with real OTLP listeners.
//!
//! Everything else in this feature's test suite calls into the library. This
//! posts actual OTLP/HTTP to a bound socket and reads the answer back over the
//! Unix socket, so it is the only test that would catch a break in the wiring
//! between them — a receiver that never reaches the collector registry, a
//! projection that never reaches the RPC response.
//!
//! The workload is the motivating one: an A/B split by a **boolean** attribute,
//! with a parent and two concurrent children, so self time and the group split
//! are both exercised on data that arrived over the wire.

#![cfg(feature = "test-support")]

use logmon_broker_core::daemon::persistence::DaemonConfig;
use logmon_broker_core::test_support::{TestClient, TestDaemonHandle};
use serde_json::{json, Value};

const BASE: u64 = 1_700_000_000_000_000_000;
const MS: u64 = 1_000_000;

/// One OTLP/JSON span. `cache_on` rides as a **BoolValue** — the encoding the
/// span matcher reads through `.as_str()` and therefore cannot see, which is
/// why `group_keys` renders attributes independently of it.
fn span(
    id: u64,
    name: &str,
    start_ns: u64,
    dur_ns: u64,
    cache_on: bool,
    parent: Option<u64>,
) -> Value {
    let mut s = json!({
        "traceId": format!("{:032x}", id / 3 + 1),
        "spanId": format!("{:016x}", id + 1),
        "name": name,
        "kind": 1,
        "startTimeUnixNano": (BASE + start_ns).to_string(),
        "endTimeUnixNano": (BASE + start_ns + dur_ns).to_string(),
        "status": { "code": 1 },
        "attributes": [
            { "key": "cache.enabled", "value": { "boolValue": cache_on } }
        ],
    });
    if let Some(p) = parent {
        s["parentSpanId"] = json!(format!("{:016x}", p + 1));
    }
    s
}

/// 10 runs, alternating cache on/off. Each run is a `handler` with two
/// concurrent `db.query` children covering half its span.
fn workload() -> Value {
    let mut spans = Vec::new();
    let mut id = 0u64;
    for run in 0..10u64 {
        let on = run % 2 == 0;
        let start = run * 100 * MS;
        let dur = if on { 20 * MS } else { 60 * MS };
        let root = id;
        spans.push(span(id, "handler", start, dur, on, None));
        id += 1;
        for _ in 0..2 {
            // Both children run [start+2ms, start+2ms+dur/2] — concurrent, so
            // their union is dur/2, not dur.
            spans.push(span(
                id,
                "db.query",
                start + 2 * MS,
                dur / 2,
                on,
                Some(root),
            ));
            id += 1;
        }
    }
    json!({
        "resourceSpans": [{
            "resource": { "attributes": [
                { "key": "service.name", "value": { "stringValue": "store_server" } }
            ]},
            "scopeSpans": [{ "spans": spans }],
        }]
    })
}

async fn otlp_http_port(d: &TestDaemonHandle) -> u16 {
    let mut client = d.connect_anon().await;
    let result: Value = client.call("status.get", json!({})).await.expect("status");
    for entry in result["receivers"].as_array().expect("receivers") {
        if let Some(rest) = entry.as_str().and_then(|s| s.strip_prefix("HTTP:")) {
            if let Ok(p) = rest.parse::<u16>() {
                return p;
            }
        }
    }
    panic!("no OTLP-HTTP receiver: {:?}", result["receivers"]);
}

async fn post_spans(port: u16, body: &Value) {
    let bytes = serde_json::to_vec(body).expect("serialisable");
    let payload = format!(
        "POST /v1/traces HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("OTLP HTTP listener is bound");
    sock.write_all(payload.as_bytes()).await.expect("headers");
    sock.write_all(&bytes).await.expect("body");
    let mut resp = String::new();
    sock.read_to_string(&mut resp).await.expect("response");
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "the daemon accepted the batch: {}",
        resp.lines().next().unwrap_or("")
    );
}

/// Spans cross an mpsc channel and a processing task, so the read has to wait
/// for them. Polls rather than sleeping a fixed amount.
async fn wait_for_matches(client: &mut TestClient, n: u64) -> Value {
    for _ in 0..200 {
        let got: Value = client
            .call(
                "collectors.get",
                json!({ "name": "ab", "group_by": "group" }),
            )
            .await
            .expect("collectors.get");
        if got["matched"].as_u64() == Some(n) {
            return got;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("only saw partial ingest after 2s");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_driving_workflow_over_a_real_daemon() {
    // On the OTLP transports, 0 means *disabled* rather than kernel-assigned
    // (it does mean kernel-assigned for GELF). So a free port is picked here
    // and handed over, which also keeps this off 4317/4318 and out of the way
    // of a developer's own daemon.
    let free = || {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        l.local_addr().expect("bound").port()
    };
    let config = DaemonConfig {
        gelf_port: 0,
        otlp_grpc_port: free(),
        otlp_http_port: free(),
        ..Default::default()
    };
    let d = TestDaemonHandle::spawn_with_real_receivers_config(config).await;
    let port = otlp_http_port(&d).await;

    let mut client = d.connect_named("perf", None).await;
    let armed: Value = client
        .call(
            "collectors.add",
            json!({
                "name": "ab",
                "filter": "sv=store_server",
                "level": "tree",
                "group_keys": ["cache.enabled"],
                "description": "read-through cache, both arms in one pass",
            }),
        )
        .await
        .expect("armed");
    assert_eq!(armed["level"], "tree");

    post_spans(port, &workload()).await;
    let got = wait_for_matches(&mut client, 30).await;

    // --- The headline numbers -------------------------------------------
    // 5 runs on: handler 20ms + 2 children 10ms each = 40ms per run  → 200ms
    // 5 runs off: handler 60ms + 2 children 30ms each = 120ms per run → 600ms
    assert_eq!(got["exact"]["count"], 30);
    assert_eq!(got["exact"]["total_ms"], 800.0);

    // --- The A/B split, on a boolean attribute --------------------------
    let groups = got["groups"].as_array().expect("grouped");
    assert_eq!(groups.len(), 2, "two arms: {groups:?}");
    let arm = |k: &str| {
        groups
            .iter()
            .find(|g| g["key"] == k)
            .unwrap_or_else(|| panic!("no `{k}` arm in {groups:?}"))
    };
    assert_eq!(arm("false")["exact"]["total_ms"], 600.0);
    assert_eq!(arm("true")["exact"]["total_ms"], 200.0);
    assert_eq!(arm("true")["exact"]["count"], 15);

    // --- Self time by clipped union -------------------------------------
    // Per "on" run: children union = [2,12] = 10ms, so handler self = 10ms,
    // plus 2 x 10ms of child self = 30ms. Five runs → 150ms.
    // Per "off" run: union = [2,32] = 30ms, handler self = 30ms, plus
    // 2 x 30ms = 90ms. Five runs → 450ms. Total 600ms.
    // Summing children instead would give 20-40 = negative for the "on" runs.
    assert_eq!(got["sampled"]["self_ms"], 600.0);
    assert_eq!(got["sampled"]["nested_matches"], 20);
    assert_eq!(got["nesting"], "detected");
    assert_eq!(got["sampled"]["complete"], true);

    // --- Ingest accounting ----------------------------------------------
    assert_eq!(got["ingest"]["drops_in_window"], 0);
    assert_eq!(got["ingest"]["shed_batches"], 0);
    assert_eq!(got["ingest"]["malformed_dropped"], 0);

    // --- The between-runs move ------------------------------------------
    let reset: Value = client
        .call("collectors.reset", json!({ "name": "ab" }))
        .await
        .expect("reset");
    assert_eq!(reset["discarded"]["matched"], 30);
    let after: Value = client
        .call("collectors.get", json!({ "name": "ab" }))
        .await
        .expect("read");
    assert_eq!(after["matched"], 0);

    // --- And the ad-hoc path sees the same spans, still buffered ---------
    let profiled: Value = client
        .call("traces.profile", json!({ "filter": "sv=store_server" }))
        .await
        .expect("profiled");
    assert_eq!(
        profiled["exact"]["total_ms"], 800.0,
        "the span store still holds the run the collector just discarded"
    );
    assert_eq!(profiled["sampled"]["self_ms"], 600.0);

    d.shutdown().await;
}
