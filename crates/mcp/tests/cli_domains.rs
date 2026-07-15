mod cli_common;

use cli_common::{spawn_with_cli, CliBuilder};

/// Run the `logmon-mcp` CLI with `args` against the test daemon and return the
/// process output. Each call is a fresh short-lived invocation, like real use.
async fn run(cli: &CliBuilder, args: &[&'static str]) -> std::process::Output {
    let mut cmd = cli.cmd();
    cmd.args(args);
    tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn domains_cli_lifecycle() {
    let (_daemon, cli) = spawn_with_cli().await;

    // create
    let out = run(&cli, &["domains", "create", "--name", "t3", "--json"]).await;
    assert!(
        out.status.success(),
        "create stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["name"], "t3");
    assert_eq!(v["source"], "ephemeral");

    // list shows t3
    let out = run(&cli, &["domains", "list", "--json"]).await;
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<&str> = v["domains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"t3"), "got {names:?}");

    // clear the bound domain (t3 via --domain)
    let out = run(&cli, &["--domain", "t3", "domains", "clear", "--json"]).await;
    assert!(
        out.status.success(),
        "clear stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["logs_cleared"], 0);
    assert_eq!(v["spans_cleared"], 0);

    // delete
    let out = run(&cli, &["domains", "delete", "--name", "t3", "--json"]).await;
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["deleted"], true);

    // gone from list
    let out = run(&cli, &["domains", "list", "--json"]).await;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<&str> = v["domains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"t3"), "t3 should be gone: {names:?}");
}

/// The global `--domain` flag must actually bind the invocation — verified via
/// `status.current_domain` reflecting the chosen domain.
#[tokio::test]
async fn domain_flag_binds_the_invocation() {
    let (_daemon, cli) = spawn_with_cli().await;
    run(&cli, &["domains", "create", "--name", "t3", "--json"]).await;

    let out = run(&cli, &["--domain", "t3", "status", "--json"]).await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["current_domain"], "t3",
        "status must reflect the --domain bind: {v}"
    );

    // An unknown --domain fails loudly rather than silently querying default.
    let out = run(&cli, &["--domain", "nope", "status", "--json"]).await;
    assert!(
        !out.status.success(),
        "--domain with an unknown domain must fail, stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
