mod cli_common;
use cli_common::{spawn_with_cli, CliBuilder};
use serde_json::{json, Value};

#[test]
fn cold_status_is_structured_and_never_spawns_a_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let stub = dir.path().join("logmon-broker");
    std::fs::write(&stub, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = CliBuilder::for_socket(dir.path().join("absent.sock"))
        .cmd()
        .env_remove("LOGMON_BROKER_SOCKET")
        .env("LOGMON_BROKER_BIN", &stub)
        .env("LOGMON_CONFIG_DIR", dir.path())
        .args([
            "--json",
            "--domain",
            "cold",
            "test-log",
            "status",
            "--run-id=lost",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["error"]["code"], "broker_unreachable");
    assert_eq!(v["error"]["domain"], "cold");
    assert_eq!(v["error"]["run_id"], "lost");
    assert!(!marker.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookup_distinguishes_idle_unknown_domain_and_missing_run_without_creating_a_domain() {
    let (_daemon, cli) = spawn_with_cli().await;
    let query = |args: &[&str], ok| {
        let out = cli.cmd().args(args).output().unwrap();
        assert_eq!(out.status.success(), ok, "{out:?}");
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    assert_eq!(
        query(
            &["--json", "--domain", "default", "test-log", "status"],
            true
        ),
        json!({"schema_version":1,"domain":"default","test_log":null})
    );
    assert_eq!(
        query(&["--json", "--domain", "default", "test-log", "list"], true)["runs"],
        json!([])
    );
    for (domain, code) in [("absent", "unknown_domain"), ("default", "run_not_found")] {
        let v = query(
            &[
                "--domain", domain, "test-log", "status", "--run-id", "lost", "--json",
            ],
            false,
        );
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["error"]["code"], code);
        assert_eq!(v["error"]["domain"], domain);
        assert_eq!(v["error"]["run_id"], "lost");
    }
    let v = query(&["--json", "--domain", "absent", "test-log", "list"], false);
    assert_eq!(v["error"]["code"], "unknown_domain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn planned_executions_accepts_zero_and_rejects_negative_counts() {
    let (_daemon, cli) = spawn_with_cli().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("progress.jsonl");
    let begin = |plan: &str| {
        cli.cmd()
            .args([
                "--json",
                "--domain",
                "default",
                "test-log",
                "begin",
                "--run-id",
                "empty-plan",
                "--jsonl-path",
                path.to_str().unwrap(),
                "--planned-executions",
                plan,
            ])
            .output()
            .unwrap()
    };
    let bad = begin("-1");
    assert!(!bad.status.success());
    assert!(!path.exists());
    let ok = begin("0");
    assert!(ok.status.success(), "{ok:?}");
    for (verb, pointer) in [
        ("status", "/test_log/executions/planned"),
        ("list", "/runs/0/executions/planned"),
    ] {
        let out = cli
            .cmd()
            .args(["--json", "--domain", "default", "test-log", verb])
            .output()
            .unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v.pointer(pointer), Some(&json!(0)), "{v}");
    }
}
