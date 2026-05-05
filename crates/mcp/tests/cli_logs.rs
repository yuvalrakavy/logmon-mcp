mod cli_common;

use cli_common::spawn_with_cli;
use logmon_broker_core::gelf::message::Level;

#[tokio::test]
async fn logs_recent_returns_injected_records() {
    let (daemon, cli) = spawn_with_cli().await;
    daemon.inject_log(Level::Info, "hello-cli").await;
    daemon.inject_log(Level::Error, "error-cli").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let output = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["logs", "recent", "--count", "10"]).output().unwrap()
    })
    .await
    .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello-cli"), "stdout: {stdout}");
    assert!(stdout.contains("error-cli"), "stdout: {stdout}");
}

#[tokio::test]
async fn logs_recent_json_returns_structured_logs() {
    let (daemon, cli) = spawn_with_cli().await;
    daemon.inject_log(Level::Info, "json-test").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let output = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["logs", "recent", "--count", "10", "--json"]).output().unwrap()
    })
    .await
    .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    let logs = v["logs"].as_array().expect("logs array");
    assert!(logs.iter().any(|l| l["message"] == "json-test"));
}

#[tokio::test]
async fn logs_recent_with_filter_passes_through() {
    let (daemon, cli) = spawn_with_cli().await;
    daemon.inject_log(Level::Info, "info-line").await;
    daemon.inject_log(Level::Error, "error-line").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let output = tokio::task::spawn_blocking(move || {
        cli.cmd()
            .args(["logs", "recent", "--filter", "l>=ERROR", "--count", "10"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("error-line"));
    assert!(!stdout.contains("info-line"), "expected only ERROR-level; got: {stdout}");
}

#[tokio::test]
async fn logs_clear_succeeds() {
    let (daemon, cli) = spawn_with_cli().await;
    daemon.inject_log(Level::Info, "to-clear").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let output = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["logs", "clear"]).output().unwrap()
    })
    .await
    .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cleared"), "stdout: {stdout}");
}

#[tokio::test]
async fn logs_recent_against_missing_broker_fails_with_guidance() {
    use assert_cmd::Command;
    use tempfile::TempDir;

    // Use a guaranteed-fresh, never-bound socket path so this test can't
    // race a parallel test or pick up stale state from a prior run.
    let tempdir = TempDir::new().unwrap();
    let bogus_socket = tempdir.path().join("nonexistent.sock");

    let mut cmd = Command::cargo_bin("logmon-mcp").unwrap();
    cmd.env("LOGMON_BROKER_SOCKET", &bogus_socket);
    let output = tokio::task::spawn_blocking(move || {
        cmd.args(["logs", "recent"]).output().unwrap()
    })
    .await
    .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("broker not running") || stderr.contains("install-service"),
        "expected fail-fast guidance; got stderr: {stderr}"
    );
}
