mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn traces_recent_empty_returns_zero_count() {
    let (_daemon, cli) = spawn_with_cli().await;
    let mut recent_cmd = cli.cmd();
    let output = tokio::task::spawn_blocking(move || {
        recent_cmd.args(["traces", "recent", "--json"]).output().unwrap()
    }).await.unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["count"], 0);
}

#[tokio::test]
async fn traces_get_unknown_trace_returns_empty() {
    let (_daemon, cli) = spawn_with_cli().await;
    let mut get_cmd = cli.cmd();
    let output = tokio::task::spawn_blocking(move || {
        get_cmd.args([
            "traces", "get",
            "--trace-id", "00000000000000000000000000000001",
            "--json",
        ]).output().unwrap()
    }).await.unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["span_count"], 0);
}

#[tokio::test]
async fn spans_context_empty_returns_zero() {
    let (_daemon, cli) = spawn_with_cli().await;
    let mut context_cmd = cli.cmd();
    let output = tokio::task::spawn_blocking(move || {
        context_cmd.args(["spans", "context", "--seq", "1", "--json"]).output().unwrap()
    }).await.unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["count"], 0);
}
