mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn filters_add_list_remove() {
    let (_daemon, cli) = spawn_with_cli().await;

    let mut add_cmd = cli.cmd();
    let add = tokio::task::spawn_blocking(move || {
        add_cmd.args(["filters", "add", "--filter", "fa=mqtt", "--description", "mqtt-only", "--json"]).output().unwrap()
    }).await.unwrap();
    assert!(add.status.success(), "add stderr: {}", String::from_utf8_lossy(&add.stderr));
    let v: serde_json::Value = serde_json::from_slice(&add.stdout).unwrap();
    let id = v["id"].as_u64().unwrap();

    let mut list_cmd = cli.cmd();
    let list = tokio::task::spawn_blocking(move || {
        list_cmd.args(["filters", "list", "--json"]).output().unwrap()
    }).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert!(v["filters"].as_array().unwrap().iter().any(|f| f["filter"] == "fa=mqtt"));

    let mut rm_cmd = cli.cmd();
    let rm = tokio::task::spawn_blocking(move || {
        rm_cmd.args(["filters", "remove", "--id", &id.to_string()]).output().unwrap()
    }).await.unwrap();
    assert!(rm.status.success());
}
