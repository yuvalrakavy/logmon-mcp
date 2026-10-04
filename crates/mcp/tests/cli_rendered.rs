//! The rendered path, end to end, on the surfaces that never exercised it.
//!
//! **This file exists because of a mutation-lens finding, not a feature.** 51 of
//! 130 mutations survived the `_display` gate, and the structural cause was one
//! sentence: *coverage tracked whether a test passed `--json`.* Every CLI test
//! for logs, traces, spans and domains did, so the rendered path was never
//! reached at any level — which is why span rendering had eleven survivors and
//! no unit test at all.
//!
//! The trap is subtler than "no test". `logs_recent_returns_injected_records`
//! runs WITHOUT `--json` and asserts `stdout.contains("hello-cli")` — which
//! passes identically whether the reply was rendered or pretty-printed, because
//! the message appears in both. A test can exercise the rendered path and still
//! be unable to tell that it did.
//!
//! So every assertion here is one that JSON would fail.

mod cli_common;

use cli_common::{display_injecting_proxy, spawn_with_cli, CliBuilder};
use logmon_broker_core::gelf::message::Level;

/// What the proxy writes into `_display`. Distinctive, so its presence in
/// stdout can only mean the client printed the injected rendering.
const INJECTED: &str = "INJECTED-RENDERING";

/// Run the CLI with `args`, returning stdout and stderr.
async fn run(cli: &CliBuilder, args: &[&str]) -> (String, String) {
    let mut cmd = cli.cmd();
    let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    let out = tokio::task::spawn_blocking(move || cmd.args(&args).output().unwrap())
        .await
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A rendered reply is not JSON, and saying so is the whole point: `contains`
/// on a value cannot distinguish the two.
///
/// **By parsing, not by looking at the first character.** The first version of
/// this helper rejected anything starting with `[` — and a block record line
/// legitimately starts `[1] `, so it failed on correctly-rendered output. A
/// check that answers a narrower question than the one asked is the same trap
/// the tests below exist to close.
fn assert_rendered(stdout: &str, what: &str) {
    assert!(
        serde_json::from_str::<serde_json::Value>(stdout).is_err(),
        "{what} came back as parseable JSON — the daemon rendered nothing, or the \
         shim did not ask:\n{stdout}"
    );
}

#[tokio::test]
async fn a_log_read_renders_records_and_its_diagnostics() {
    let (daemon, cli) = spawn_with_cli().await;
    daemon.inject_log(Level::Info, "rendered-marker").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let out = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["logs", "recent", "--count", "10"]).output().unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    assert_rendered(&stdout, "logs recent");
    // The block form: `[seq] timestamp LEVEL message`. A JSON reply has the
    // message but none of this shape.
    assert!(stdout.contains("INFO"), "the level is upper-cased: {stdout}");
    assert!(stdout.contains("rendered-marker"), "{stdout}");
    assert!(
        stdout.lines().next().unwrap_or("").starts_with('['),
        "a record line leads with its seq: {stdout}"
    );
    // The structural drop rule, over the wire: every key but the records.
    assert!(stdout.contains("scanned="), "diagnostics dropped: {stdout}");
    assert!(stdout.contains("buffer_total="), "{stdout}");
    // …and the records are NOT re-emitted as JSON in that tail.
    assert_eq!(
        stdout.matches("rendered-marker").count(),
        1,
        "the record was rendered AND dumped: {stdout}"
    );
}

/// An empty read over a live buffer is the case an agent most easily
/// misreads — and the note that prevents it is computed, not carried.
#[tokio::test]
async fn an_empty_filtered_read_says_records_are_flowing() {
    let (daemon, cli) = spawn_with_cli().await;
    for i in 0..3 {
        daemon.inject_log(Level::Info, &format!("present-{i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let out = tokio::task::spawn_blocking(move || {
        cli.cmd()
            .args(["logs", "recent", "--filter", "no-such-substring-anywhere"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    assert!(stdout.contains("(no logs)"), "{stdout}");
    assert!(
        stdout.contains("records ARE flowing"),
        "an empty read over a live buffer read as a quiet system: {stdout}"
    );
}

#[tokio::test]
async fn a_domain_list_renders_as_a_padded_table() {
    let (_daemon, cli) = spawn_with_cli().await;

    let out = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["domains", "list"]).output().unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    assert_rendered(&stdout, "domains list");
    let header = stdout.lines().next().unwrap_or("");
    assert!(header.starts_with("| name"), "a header row: {stdout}");
    // The column set is pinned, so it cannot be quietly narrowed: a mutation
    // dropping six of eight columns passed the whole suite before this.
    for col in ["name", "src", "logs", "spans", "oldest", "newest", "idle_s", "stale"] {
        assert!(header.contains(col), "column `{col}` missing: {header}");
    }
    assert!(
        stdout.lines().nth(1).unwrap_or("").starts_with("|---"),
        "a separator row: {stdout}"
    );
    assert!(stdout.contains("| default "), "{stdout}");
}

/// Filters and triggers render as BLOCKS, not tables, so the DSL is not
/// escaped — a `|` inside a regex is the alternation operator, and a reader who
/// copies `\|` back registers something different from what is running.
#[tokio::test]
async fn a_trigger_shows_its_filter_verbatim_not_markdown_escaped() {
    let (_daemon, cli) = spawn_with_cli().await;
    let add = tokio::task::spawn_blocking({
        let cli = cli.cmd();
        move || {
            let mut c = cli;
            c.args([
                "triggers", "add", "--filter", "/panic|unwrap failed/", "--description", "alt",
            ])
            .output()
            .unwrap()
        }
    })
    .await
    .unwrap();
    assert!(add.status.success(), "{}", String::from_utf8_lossy(&add.stderr));

    let out = tokio::task::spawn_blocking(move || {
        cli.cmd().args(["triggers", "list"]).output().unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    assert_rendered(&stdout, "triggers list");
    assert!(
        stdout.contains("/panic|unwrap failed/"),
        "the regex must render verbatim: {stdout}"
    );
    assert!(
        !stdout.contains(r"panic\|unwrap"),
        "the escape is visible on a surface that does not decode markdown, so a \
         copied filter would match one literal string instead of two: {stdout}"
    );
}

/// The one method whose whole purpose is dropping noise the client already has.
#[tokio::test]
async fn status_counts_the_tools_rather_than_listing_them() {
    let (_daemon, cli) = spawn_with_cli().await;

    let out = tokio::task::spawn_blocking(move || {
        cli.cmd().arg("status").output().unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    assert_rendered(&stdout, "status");
    assert!(stdout.contains("tools: "), "{stdout}");
    assert!(stdout.contains(" served"), "{stdout}");
    assert!(
        !stdout.contains("add_bookmark"),
        "a tool name survived into the rendering: {stdout}"
    );
    // The line that is absent exactly when it matters most.
    assert!(
        stdout.contains("last traffic:"),
        "a broker that has received nothing must still say so: {stdout}"
    );
}

/// `--json` is the escape hatch and must stay clean: a script piping into `jq`
/// gets the result, with no rendered field to strip.
#[tokio::test]
async fn json_mode_carries_no_rendered_string_on_any_surface() {
    let (daemon, cli) = spawn_with_cli().await;
    daemon.inject_log(Level::Info, "json-clean").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    for args in [
        vec!["logs", "recent", "--json"],
        vec!["domains", "list", "--json"],
        vec!["triggers", "list", "--json"],
        vec!["status", "--json"],
    ] {
        let label = args.join(" ");
        let cmd = cli.cmd();
        let out = tokio::task::spawn_blocking(move || {
            let mut c = cmd;
            c.args(&args).output().unwrap()
        })
        .await
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert!(
            v.get("_display").is_none(),
            "`{label}` asked for a rendering it will never print: {v}"
        );
    }
}

/// `logs export` with no `--path` prints the RENDERING, not its content field.
///
/// The content field is `logs`, an ARRAY. The body rule prints a content field
/// only when it is a string — a document — and an earlier version printed this
/// one as a JSON list and returned before the rendering could be seen. Nothing
/// ran `logs export` without a path, so the rule had no test.
#[tokio::test]
async fn logs_export_with_no_path_prints_the_rendering_not_the_record_array() {
    let (daemon, cli) = spawn_with_cli().await;
    daemon.inject_log(Level::Info, "export-marker").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let (stdout, stderr) = run(&cli, &["logs", "export"]).await;

    assert_rendered(&stdout, "logs export");
    let record = stdout
        .lines()
        .find(|l| l.contains("export-marker"))
        .unwrap_or_else(|| panic!("the record is missing: {stdout}\n{stderr}"));
    assert!(
        record.starts_with('[') && record.ends_with("INFO  export-marker"),
        "a block record line, not a JSON element: {record:?}"
    );
}

/// `--json` prints JSON, even when the reply carries a rendering nobody asked
/// for — from a daemon that renders unasked, say.
///
/// **This pins `emit()`'s own `!json` guard, independently of `want_display`.**
/// Against the real daemon the guard is unreachable: `--json` never asks for a
/// rendering, so no `_display` arrives for it to skip, and deleting it left the
/// suite green. The proxy delivers one regardless.
#[tokio::test]
async fn json_mode_prints_json_even_when_a_reply_carries_a_rendering_unasked() {
    let (daemon, _) = spawn_with_cli().await;
    daemon.inject_log(Level::Info, "json-guard").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let (_dir, proxy) =
        display_injecting_proxy(daemon.socket_path.clone(), &["logs.recent"], INJECTED).await;
    let cli = CliBuilder::for_socket(proxy);

    let (stdout, stderr) = run(&cli, &["logs", "recent", "--json"]).await;

    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("`--json` printed something that is not JSON ({e}):\n{stdout}\n{stderr}")
    });
    assert!(
        v.get("logs").is_some(),
        "the result, not something else: {v}"
    );
    // Without this the test proves nothing: a proxy that injected nothing
    // would leave the guard with nothing to do, and the test green either way.
    assert_eq!(
        v.get("_display").and_then(|d| d.as_str()),
        Some(INJECTED),
        "the reply never carried the rendering, so the guard was not exercised: {v}"
    );
}

/// A document tool given no `--path` prints the DOCUMENT, even when the same
/// reply carries a rendering.
///
/// No method has both a renderer and a string content field today, so which
/// one wins is unobservable against the real daemon — and the first renderer
/// added to a document tool would have decided it silently. The proxy makes
/// `collectors.document` carry both.
#[tokio::test]
async fn a_document_outranks_a_rendering_of_the_same_reply() {
    let (daemon, _) = spawn_with_cli().await;
    let (_dir, proxy) = display_injecting_proxy(
        daemon.socket_path.clone(),
        &["collectors.document", "collectors.list"],
        INJECTED,
    )
    .await;
    let cli = CliBuilder::for_socket(proxy);

    for args in [
        &[
            "collectors",
            "add",
            "--name",
            "e",
            "--filter",
            "ALL",
            "--level",
            "tree",
        ][..],
        &["collectors", "snapshot", "--name", "e", "--label", "base"][..],
    ] {
        let (stdout, stderr) = run(&cli, args).await;
        assert!(!stdout.is_empty(), "`{}` failed: {stderr}", args.join(" "));
    }

    // The proxy does inject: a method with no content field prints it.
    let (listed, _) = run(&cli, &["collectors", "list"]).await;
    assert_eq!(
        listed.trim(),
        INJECTED,
        "the proxy injected nothing: {listed}"
    );

    let (doc, stderr) = run(&cli, &["collectors", "document", "e@base"]).await;
    let head = doc.lines().next().unwrap_or("");
    assert!(
        doc.starts_with("---"),
        "the markdown document, not the rendering — got {head:?}\n{stderr}"
    );
    assert!(
        !doc.contains(INJECTED),
        "the rendering displaced the document — got {head:?}"
    );
}
