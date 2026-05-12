mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn sessions_list_includes_cli_session() {
    let (_daemon, cli) = spawn_with_cli().await;
    let output = tokio::task::spawn_blocking(move || {
        cli.cmd()
            .args(["sessions", "list", "--json"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let sessions = v["sessions"].as_array().unwrap();
    assert!(
        sessions.iter().any(|s| s["name"] == "cli"),
        "expected named session 'cli'; got: {v}"
    );
}
