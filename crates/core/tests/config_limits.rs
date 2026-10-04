//! A configured buffer size no ring could reserve is refused at startup — not on a domain's
//! first record, where reserving the ring would abort the whole process. A global size stops
//! the daemon (the `default` domain is built from it); a config-declared domain's own size
//! skips that domain, as any other bad domain entry is skipped, and the daemon starts.
#![cfg(feature = "test-support")]

use logmon_broker_core::daemon::persistence::{ConfigDomain, DaemonConfig, MAX_BUFFER_SIZE};
use logmon_broker_core::daemon::server::{run_with_overrides, DaemonOverrides};
use logmon_broker_core::test_support::*;
use serde_json::{json, Value};

#[tokio::test]
async fn the_daemon_refuses_to_start_with_an_oversize_global_buffer() {
    let dir = tempfile::tempdir().unwrap();
    // Nothing here can reach the live broker even if the check were missing: no receivers
    // (an injected log channel), a private config dir and socket, and a shutdown that has
    // already fired, so a daemon that did start would stop at once and return Ok.
    let (_log_tx, log_rx) = tokio::sync::mpsc::channel(1);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    shutdown_tx.send(()).unwrap();
    let config = DaemonConfig {
        buffer_size: MAX_BUFFER_SIZE + 1,
        ..DaemonConfig::default()
    };
    let result = run_with_overrides(
        config,
        DaemonOverrides {
            config_dir: Some(dir.path().to_path_buf()),
            socket_path: Some(dir.path().join("logmon.sock")),
            injected_log_rx: Some(log_rx),
            shutdown_rx: Some(shutdown_rx),
            accept_paused: None,
            skip_tracing_init: true,
        },
    )
    .await;
    let err = result
        .expect_err("an oversize global buffer must stop the daemon")
        .to_string();
    assert!(err.contains("`buffer_size`"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_config_domain_with_an_oversize_buffer_is_skipped_and_the_daemon_starts() {
    let declared = |name: &str, log: Option<usize>, span: Option<usize>| ConfigDomain {
        name: name.into(),
        gelf_port: Some(0),
        otlp_grpc_port: Some(0),
        otlp_http_port: Some(0),
        log_buffer_size: log,
        span_buffer_size: span,
    };
    let mut config = default_test_config();
    config.domains = vec![
        declared("ok", None, None),
        declared("big", Some(MAX_BUFFER_SIZE + 1), None),
        declared("bigspans", None, Some(MAX_BUFFER_SIZE + 1)),
        // An oversize entry does not claim its name: the corrected one after it starts.
        declared("again", Some(MAX_BUFFER_SIZE + 1), None),
        declared("again", None, None),
    ];

    let daemon = TestDaemonHandle::spawn_with_config(config).await;
    let mut client = daemon.connect_anon().await;
    let listed: Value = client.call("domains.list", json!({})).await.unwrap();
    let text = listed.to_string();
    assert!(
        text.contains("\"ok\""),
        "vacuity: the good entry is up: {text}"
    );
    assert!(
        !text.contains("\"big\""),
        "the oversize entry is skipped: {text}"
    );
    assert!(
        !text.contains("\"bigspans\""),
        "an oversize SPAN buffer is skipped too: {text}"
    );
    assert!(
        text.contains("\"again\""),
        "a corrected entry after an oversize one of the same name starts: {text}"
    );
}

/// A trigger's `pre_window` sizes the pre-trigger buffer, which holds a clone of every
/// record up to it — so it has the buffer limit too, on add and on edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_window_above_the_buffer_limit_is_refused() {
    let daemon = TestDaemonHandle::spawn_with_config(default_test_config()).await;
    let mut client = daemon.connect_anon().await;
    let too_big = MAX_BUFFER_SIZE as u64 + 1;

    let err = client
        .call::<Value>(
            "triggers.add",
            json!({ "filter": "l>=ERROR", "pre_window": too_big }),
        )
        .await
        .expect_err("refused on add");
    assert!(format!("{err:?}").contains("pre_window"), "{err:?}");

    let added: Value = client
        .call(
            "triggers.add",
            json!({ "filter": "l>=ERROR", "pre_window": 10 }),
        )
        .await
        .unwrap();
    let err = client
        .call::<Value>(
            "triggers.edit",
            json!({ "id": added["id"], "pre_window": too_big }),
        )
        .await
        .expect_err("refused on edit");
    assert!(format!("{err:?}").contains("pre_window"), "{err:?}");
}
