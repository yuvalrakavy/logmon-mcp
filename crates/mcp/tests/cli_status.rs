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
    // The rendering is the DAEMON's now — this asserts the whole path, from the
    // shim asking for it to the string reaching stdout. It used to assert a
    // hand-written shim format that said "uptime:"; that renderer is gone, and
    // this test failing was the first proof the new one had taken over.
    assert!(stdout.contains("broker "), "stdout: {stdout}");
    assert!(stdout.contains(", up "), "stdout: {stdout}");
    assert!(stdout.contains("receivers"), "stdout: {stdout}");
    assert!(
        !stdout.trim_start().starts_with('{'),
        "the reply came back as raw JSON, so the daemon rendered nothing or the \
         shim ignored it: {stdout}"
    );
    // A broker with nothing bound says so rather than trailing off.
    assert!(
        stdout.contains("(none bound") || stdout.contains("receivers: UDP"),
        "stdout: {stdout}"
    );
}

/// `--json` must not carry the rendering: a script piping into `jq` should get
/// exactly the result, with no multi-KB text field to strip.
#[tokio::test]
async fn status_json_carries_no_rendered_string() {
    let (_daemon, cli) = spawn_with_cli().await;

    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = cli.cmd();
        cmd.args(["status", "--json"])
            .output()
            .expect("subprocess failed")
    })
    .await
    .unwrap();

    let v: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json parse");
    assert!(
        v.get("_display").is_none(),
        "`--json` asked for a rendering it will never print: {v}"
    );
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
