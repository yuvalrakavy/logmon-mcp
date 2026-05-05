mod cli_common;

use cli_common::spawn_with_cli;

#[tokio::test]
async fn bookmarks_add_then_list_round_trip() {
    let (_daemon, cli) = spawn_with_cli().await;

    let mut add_cmd = cli.cmd();
    let add = tokio::task::spawn_blocking(move || {
        add_cmd.args(["bookmarks", "add", "--name", "my-anchor", "--start-seq", "42"]).output().unwrap()
    }).await.unwrap();
    assert!(add.status.success(), "add failed: {}", String::from_utf8_lossy(&add.stderr));
    let add_stdout = String::from_utf8_lossy(&add.stdout);
    assert!(add_stdout.contains("my-anchor") || add_stdout.contains("seq=42"), "got: {add_stdout}");

    let mut list_cmd = cli.cmd();
    let list = tokio::task::spawn_blocking(move || {
        list_cmd.args(["bookmarks", "list", "--json"]).output().unwrap()
    }).await.unwrap();
    assert!(list.status.success());
    let v: serde_json::Value = serde_json::from_slice(&list.stdout).expect("json");
    let entries = v["bookmarks"].as_array().expect("bookmarks array");
    assert!(entries.iter().any(|b| b["seq"] == 42), "expected anchor with seq=42; got: {v}");
}

#[tokio::test]
async fn bookmarks_remove_succeeds() {
    let (_daemon, cli) = spawn_with_cli().await;

    let mut add_cmd = cli.cmd();
    let _ = tokio::task::spawn_blocking(move || {
        add_cmd.args(["bookmarks", "add", "--name", "to-rm"]).output().unwrap()
    }).await.unwrap();

    let mut rm_cmd = cli.cmd();
    let rm = tokio::task::spawn_blocking(move || {
        rm_cmd.args(["bookmarks", "remove", "--name", "cli/to-rm"]).output().unwrap()
    }).await.unwrap();
    assert!(rm.status.success(), "remove failed: {}", String::from_utf8_lossy(&rm.stderr));
}

#[tokio::test]
async fn bookmarks_add_replace_overwrites() {
    let (_daemon, cli) = spawn_with_cli().await;

    let mut first_cmd = cli.cmd();
    let _ = tokio::task::spawn_blocking(move || {
        first_cmd.args(["bookmarks", "add", "--name", "x", "--start-seq", "1"]).output().unwrap()
    }).await.unwrap();

    // Without --replace, second add errors.
    let mut dup_cmd = cli.cmd();
    let dup = tokio::task::spawn_blocking(move || {
        dup_cmd.args(["bookmarks", "add", "--name", "x", "--start-seq", "2"]).output().unwrap()
    }).await.unwrap();
    assert!(!dup.status.success());

    // With --replace, succeeds.
    let mut replace_cmd = cli.cmd();
    let ok = tokio::task::spawn_blocking(move || {
        replace_cmd.args(["bookmarks", "add", "--name", "x", "--start-seq", "2", "--replace"]).output().unwrap()
    }).await.unwrap();
    assert!(ok.status.success(), "replace failed: {}", String::from_utf8_lossy(&ok.stderr));
}
