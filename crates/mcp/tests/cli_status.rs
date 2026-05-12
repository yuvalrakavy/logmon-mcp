mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn status_human_format_includes_uptime() {
    let (_daemon, cli) = spawn_with_cli().await;

    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = cli.cmd();
        cmd.arg("status").output().expect("subprocess failed")
    })
    .await
    .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("uptime"), "stdout: {stdout}");
    assert!(stdout.contains("receivers"), "stdout: {stdout}");
}

#[tokio::test]
async fn status_json_emits_typed_struct() {
    let (_daemon, cli) = spawn_with_cli().await;

    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = cli.cmd();
        cmd.args(["status", "--json"])
            .output()
            .expect("subprocess failed")
    })
    .await
    .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json parse");
    assert!(v.get("daemon_uptime_secs").is_some(), "got: {v}");
    assert!(v.get("receivers").is_some(), "got: {v}");
}
