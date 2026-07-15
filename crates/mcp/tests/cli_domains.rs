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

/// Surfacing-gate finding 1 (CRITICAL): `--domain` must NOT stick across
/// invocations. The CLI connects with a persistent NAMED session ("cli"), so a
/// prior `--domain X` bind lives on server-side; an unflagged invocation must
/// reset to `default` rather than silently keep serving X. Both invocations here
/// share ONE daemon + the "cli" session, reproducing the real deployment.
#[tokio::test]
async fn domain_flag_does_not_stick_across_invocations() {
    let (_daemon, cli) = spawn_with_cli().await;
    run(&cli, &["domains", "create", "--name", "t3", "--json"]).await;

    // Invocation 1: bind to t3.
    let out = run(&cli, &["--domain", "t3", "status", "--json"]).await;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["current_domain"], "t3");

    // Invocation 2: UNFLAGGED — must be back on default, not stuck on t3.
    let out = run(&cli, &["status", "--json"]).await;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["current_domain"], "default",
        "an unflagged invocation must reset to default, not stick on the prior --domain"
    );
}

/// Surfacing-gate finding 2: registry-management verbs (create/delete/list) are
/// domain-agnostic, so `--domain` must NOT gate them — else creating the very
/// domain named in `--domain` is impossible (bind-before-create can't succeed).
#[tokio::test]
async fn domain_flag_does_not_block_creating_that_domain() {
    let (_daemon, cli) = spawn_with_cli().await;
    let out = run(
        &cli,
        &[
            "--domain", "t3", "domains", "create", "--name", "t3", "--json",
        ],
    )
    .await;
    assert!(
        out.status.success(),
        "--domain t3 domains create t3 must succeed (registry op ignores the bind), stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["name"], "t3");
}

/// Consumer #1: `LOGMON_DOMAIN` (set once per worktree) auto-scopes every
/// invocation with no `--domain`; an explicit `--domain` overrides it.
#[tokio::test]
async fn logmon_domain_env_scopes_the_invocation() {
    let (_daemon, cli) = spawn_with_cli().await;
    run(&cli, &["domains", "create", "--name", "t3", "--json"]).await;
    run(&cli, &["domains", "create", "--name", "t4", "--json"]).await;

    // LOGMON_DOMAIN set, no --domain → auto-binds t3.
    let mut cmd = cli.cmd();
    cmd.env("LOGMON_DOMAIN", "t3").args(["status", "--json"]);
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["current_domain"], "t3",
        "LOGMON_DOMAIN must scope an unflagged invocation"
    );

    // --domain overrides LOGMON_DOMAIN.
    let mut cmd = cli.cmd();
    cmd.env("LOGMON_DOMAIN", "t3")
        .args(["--domain", "t4", "status", "--json"]);
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["current_domain"], "t4",
        "--domain must override LOGMON_DOMAIN"
    );
}
