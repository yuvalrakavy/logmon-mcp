mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn triggers_add_then_list() {
    let (_daemon, cli) = spawn_with_cli().await;

    let mut add_cmd = cli.cmd();
    let add = tokio::task::spawn_blocking(move || {
        add_cmd
            .args([
                "triggers",
                "add",
                "--filter",
                "l>=ERROR",
                "--oneshot",
                "--description",
                "test",
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        add.status.success(),
        "add stderr: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let mut list_cmd = cli.cmd();
    let list = tokio::task::spawn_blocking(move || {
        list_cmd
            .args(["triggers", "list", "--json"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(list.status.success());
    let v: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let entries = v["triggers"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|t| t["filter"] == "l>=ERROR" && t["oneshot"] == true),
        "expected oneshot ERROR trigger; got: {v}"
    );
}

#[tokio::test]
async fn triggers_remove_succeeds() {
    let (_daemon, cli) = spawn_with_cli().await;

    let mut add_cmd = cli.cmd();
    let add = tokio::task::spawn_blocking(move || {
        add_cmd
            .args(["triggers", "add", "--filter", "l>=WARN", "--json"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&add.stdout).unwrap();
    let id = v["id"].as_u64().unwrap();

    let mut rm_cmd = cli.cmd();
    let rm = tokio::task::spawn_blocking(move || {
        rm_cmd
            .args(["triggers", "remove", "--id", &id.to_string()])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        rm.status.success(),
        "rm stderr: {}",
        String::from_utf8_lossy(&rm.stderr)
    );
}
