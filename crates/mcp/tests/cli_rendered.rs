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

use cli_common::spawn_with_cli;
use logmon_broker_core::gelf::message::Level;

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
