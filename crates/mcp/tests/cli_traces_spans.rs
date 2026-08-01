mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn traces_recent_empty_returns_zero_count() {
    let (_daemon, cli) = spawn_with_cli().await;
    let mut recent_cmd = cli.cmd();
    let output = tokio::task::spawn_blocking(move || {
        recent_cmd
            .args(["traces", "recent", "--json"])
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
    assert_eq!(v["count"], 0);
}

#[tokio::test]
async fn traces_get_unknown_trace_returns_empty() {
    let (_daemon, cli) = spawn_with_cli().await;
    let mut get_cmd = cli.cmd();
    let output = tokio::task::spawn_blocking(move || {
        get_cmd
            .args([
                "traces",
                "get",
                "--trace-id",
                "00000000000000000000000000000001",
                "--json",
            ])
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
    assert_eq!(v["span_count"], 0);
}

#[tokio::test]
async fn spans_context_empty_returns_zero() {
    let (_daemon, cli) = spawn_with_cli().await;
    let mut context_cmd = cli.cmd();
    let output = tokio::task::spawn_blocking(move || {
        context_cmd
            .args(["spans", "context", "--seq", "1", "--json"])
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
    assert_eq!(v["count"], 0);
}

/// The reproduction from #8, at the layer it was reported from.
///
/// The handler tests assert the daemon builds the right sentence. This asserts
/// a person typing the command actually receives it — the CLI is assembled from
/// the broker's manifest and forwards the daemon's text, and a rejection that
/// never reaches stderr is the same as no rejection at all.
///
/// `traces profile` is used because it needs no armed collector and no named
/// session, and it reads `group_by` through the same `profile_options` as
/// `collectors get`, which is where the message is built.
#[tokio::test]
async fn a_rejected_group_by_reaches_the_person_who_typed_it() {
    let (_daemon, cli) = spawn_with_cli().await;
    let mut cmd = cli.cmd();
    let output = tokio::task::spawn_blocking(move || {
        cmd.args(["traces", "profile", "--group-by", "nonsense"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    assert!(
        !output.status.success(),
        "an unknown group_by must fail the command, not print a result"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // `none` is the one the message used to omit, and the only spelling for
    // the ungrouped overall figures.
    for spelling in ["none", "name", "group", "trace", "path"] {
        assert!(
            stderr.contains(&format!("`{spelling}`")),
            "the CLI must surface `{spelling}` as an accepted value: {stderr}"
        );
    }
}
