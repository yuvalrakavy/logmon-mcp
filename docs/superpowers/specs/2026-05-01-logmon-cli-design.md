# logmon-mcp CLI mode — design

**Date:** 2026-05-01
**Status:** approved (pending user review of this document)
**Branch target:** new feature branch off `main` after the cursor work merges.

## Summary

Add a CLI mode to the existing `logmon-mcp` binary so Claude Code (and any other shell consumer) can interact with the running broker through `Bash`-tool invocations instead of through MCP tool calls. Default invocation (`logmon-mcp` with no subcommand) preserves today's MCP stdio behavior; new subcommands route through the typed SDK to mirror the full MCP tool surface.

Result: one binary, two modes. Existing `claude mcp add logmon -- logmon-mcp` registrations keep working unchanged. Subagents launched via the Agent tool — which don't inherit MCP servers — gain broker access via `Bash("logmon-mcp logs recent ...")`. CLI mode also serves as a fallback when the MCP server disconnects mid-session, and as a stable interface for non-Claude consumers (Codex, scripts, CI).

No new daemon dependencies, no new server-side surface. The CLI is a SDK consumer, same dep tree as the MCP shim.

## Motivation

Three concrete pain points the CLI mode addresses:

1. **Subagents can't use MCP.** Subagents launched via the Agent tool don't inherit MCP servers. Today, broker work in a subagent has to be relayed through the parent agent. A CLI binary works in any subagent unchanged.

2. **MCP servers disconnect mid-session.** Observed during the cursor implementation work: `mcp__logmon__*` tools went "no longer available — their MCP server disconnected." When that happens with MCP-only access, Claude is locked out until the user reloads the session. CLI mode always works as long as the broker socket is up.

3. **Distribution friction.** MCP requires `claude mcp add` per-project. CLI is `cargo install` once and then available from any shell, any agent, any project. Removes the registration step from the path.

Secondary benefits:

- **Output piping.** CLI output flows through pipes — `logmon-mcp logs recent | head -20`, `| jq '.logs[].message'`, `| grep ERROR`. MCP tools return whole result sets that get dumped into the response.
- **Stable surface for non-Claude consumers.** `store-test` integration, deployment scripts, monitoring crons all use the same binary.

## Non-goals

- **Notifications / push.** No `wait-for-trigger` or `tail -f` subcommand in v1. MCP keeps that niche. CLI is for query and management. If a polling pattern works well enough later, we can add a long-running subcommand, but YAGNI for now.
- **Operator-grade ops.** No `force-sweep`, no `dump-state`, no `restart-without-data-loss`. Lifecycle is `logmon-broker`'s job.
- **Auto-starting the broker.** Each CLI invocation is short-lived; spawning a broker per call is wasteful and confusing. CLI errors fast with guidance ("broker not running; start with...") if the socket is missing.

## Design

### Architecture

Single binary, dual mode. `crates/mcp/src/main.rs` adds a top-level subcommand enum to the existing `clap::Parser`-based CLI:

```rust
#[derive(Parser)]
#[command(name = "logmon-mcp", about = "logmon broker MCP shim and CLI tool")]
struct Cli {
    #[command(subcommand)]
    command: Option<Subcommand>,

    // Existing global flags (unchanged) for MCP stdio mode override
    #[arg(long)]
    session: Option<String>,

    // New global flag for output format
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Subcommand {
    Logs(LogsCmd),
    Bookmarks(BkmCmd),
    Triggers(TrgCmd),
    Filters(FltCmd),
    Traces(TrcCmd),
    Spans(SpnCmd),
    Sessions(SesCmd),
    Status,
}
```

When `command` is `None`, `main()` runs today's MCP stdio entry point unchanged (`run_mcp_server(..)`). When `command` is `Some(...)`, `main()` connects to the broker via the SDK and dispatches to the matching CLI handler.

CLI handlers live in a new `crates/mcp/src/cli/` module, one file per subcommand group. Each handler:

1. Parses its args from clap into the corresponding protocol param struct (`LogsRecent`, `BookmarksAdd`, etc.).
2. Calls the typed SDK method (`broker.logs_recent(...)`, `broker.bookmarks_add(...)`).
3. Formats the typed result via shared format helpers (`format::table(...)`, `format::block(...)`, `format::json(...)`).

### Subcommand structure

Grouped by namespace, mirroring the MCP tool families. The mapping from MCP tool to CLI subcommand:

| MCP tool | CLI invocation |
|---|---|
| `get_recent_logs` | `logmon-mcp logs recent --count 50 --filter '...'` |
| `get_log_context` | `logmon-mcp logs context --seq 12345 --before 10 --after 10` |
| `export_logs` | `logmon-mcp logs export --filter '...' [--out FILE]` (see "`--out FILE` semantics" below) |
| `clear_logs` | `logmon-mcp logs clear` |
| `add_bookmark` | `logmon-mcp bookmarks add --name foo [--start-seq N] [--description "..."] [--replace]` |
| `list_bookmarks` | `logmon-mcp bookmarks list [--session NAME]` |
| `remove_bookmark` | `logmon-mcp bookmarks remove --name foo` |
| `clear_bookmarks` | `logmon-mcp bookmarks clear [--session NAME]` |
| `add_trigger` | `logmon-mcp triggers add --filter '...' [--oneshot] [--pre-window N] [--post-window N] [--notify-context N] [--description "..."]` |
| `get_triggers` | `logmon-mcp triggers list` |
| `edit_trigger` | `logmon-mcp triggers edit --id N [--filter '...'] ...` |
| `remove_trigger` | `logmon-mcp triggers remove --id N` |
| `add_filter` | `logmon-mcp filters add --filter '...' [--description "..."]` |
| `get_filters` | `logmon-mcp filters list` |
| `edit_filter` | `logmon-mcp filters edit --id N [--filter '...'] [--description "..."]` |
| `remove_filter` | `logmon-mcp filters remove --id N` |
| `get_recent_traces` | `logmon-mcp traces recent --count 20 [--filter '...']` |
| `get_trace` | `logmon-mcp traces get --trace-id HEX [--include-logs] [--filter '...']` |
| `get_trace_summary` | `logmon-mcp traces summary --trace-id HEX` |
| `get_slow_spans` | `logmon-mcp traces slow [--min-duration-ms N] [--count N] [--filter '...'] [--group-by name]` |
| `get_trace_logs` | `logmon-mcp traces logs --trace-id HEX [--filter '...']` |
| `get_span_context` | `logmon-mcp spans context --seq N [--before N] [--after N]` |
| `get_sessions` | `logmon-mcp sessions list` |
| `drop_session` | `logmon-mcp sessions drop --name NAME` |
| `get_status` | `logmon-mcp status` |

25 MCP tools mapped to 25 CLI subcommands across 7 groups. The mapping is mechanical; no MCP tool is omitted, no CLI subcommand has functionality that isn't in MCP.

**Triggers and notifications.** Triggers added via the CLI persist in the named session, but the CLI invocation exits before any matching log can fire the trigger. CLI is for trigger *management* — add, list, edit, remove. To actually receive trigger fires, subscribe via the long-running MCP shim (which holds a connection open and forwards `Notification::TriggerFired` to the MCP client) or via a custom SDK consumer that calls `Broker::subscribe_notifications()`. A user typing `logmon-mcp triggers add --filter ... --oneshot` and expecting the CLI to block until the fire arrives will not get that behavior; document this constraint in the subcommand's help text and in the README.

**`--out FILE` semantics (logs export).** `--out FILE` redirects the export's output to `FILE` instead of stdout. The format follows `--json`: with `--json`, JSON is written to `FILE`; without, the human-readable block format is written to `FILE`. `--out -` (single dash) is stdout (POSIX convention); equivalent to omitting the flag. If `FILE` already exists, it is overwritten. No append mode in v1.

### Output format

**Default: human-readable text.** Optimized for Claude reading the output via the Bash tool, secondarily for human terminal use.

- **List-shaped results** (`bookmarks list`, `triggers list`, `filters list`, `sessions list`, `traces recent`, `traces slow --group-by`): rendered as aligned tables. Borders kept minimal; one record per row.
- **Log/span entry lists** (`logs recent`, `logs context`, `logs export`, `traces logs`, `spans context`): one record per block, formatted as:
  ```
  [seq=12345] 2026-05-01T18:42:13.481Z INFO mqtt: connection established
    file=src/mqtt.rs:142 host=mqtt.example.com
  ```
  (Two lines per record: header line with seq + timestamp + level + facility + message; second line with secondary fields when present.)
- **Single-record results** (`status`, `traces get`, `traces summary`): block format with key=value lines, headers for nested structures.
- **Action confirmations** (`bookmarks add`, `triggers add`, `filters add`, etc.): one-line summary like `bookmark added: cli/foo (seq=12345)` or `trigger added: id=42 oneshot=true`.

**Truncation.** Claude's Bash output budget is finite (typically on the order of tens of thousands of characters per tool invocation). When a result list would overflow, the formatter renders the first N records and appends `... 47 more records, use --json or refine the filter to see them`. Threshold is per-format-helper; tables truncate at row count (~50 rows), blocks at character count (~16 KB by default — leaves headroom under typical Bash caps). Both thresholds are tunable constants in `crates/mcp/src/cli/format.rs` so CI or operator workflows that need different limits can override at build time if necessary; not exposed as user flags in v1.

**`--json` flag.** Emits the typed protocol result struct serialized as pretty JSON to stdout. No truncation, no formatting cleverness. Pipes cleanly into `jq`. On error, emits `{"error": "<message>"}` to stdout and exits non-zero.

### Session model

CLI invocations connect to the broker as named sessions by default, using the session name `"cli"`. This means:

- `logmon-mcp bookmarks add foo` adds a bookmark in the `cli` session.
- A subsequent `logmon-mcp bookmarks list` (any shell, any time, **same OS user** — the broker socket at `~/.config/logmon/logmon.sock` is per-user) finds the bookmark.
- Triggers and filters added via CLI persist across CLI invocations and broker restarts (named-session persistence).

**Cooperative multi-tenancy with MCP shims.** If a user has registered an MCP shim with the same session name (e.g. `claude mcp add logmon -- logmon-mcp --session cli`), the CLI shares state with that shim cooperatively. This is by design, not a collision: both surfaces operate on the same per-session triggers/filters/bookmarks. To isolate, pass `--session NAME` to either or both.

Override with the global `--session NAME` flag for isolation. Use cases for override:

- Per-test cursor names: `logmon-mcp --session test-foo-run-abc bookmarks add ...`
- Looking at another session's state: `logmon-mcp --session some-other bookmarks list`

Concurrent CLI invocations to the same session work via the broker's existing named-session-reconnect flow: the first invocation connects + holds + disconnects on exit; the second invocation finds the session disconnected and reconnects. The brief overlap window where one is finishing and another starting is handled by `SessionRegistry::create_named` returning `AlreadyExists`, which the SDK retries via the existing reconnect logic.

`client_info` is set on every CLI invocation to:

```json
{
    "name": "logmon-mcp",
    "version": "<pkg version>",
    "mode": "cli",
    "argv": ["logs", "recent"]
}
```

`argv` contains ONLY the subcommand path — group + verb (e.g. `["logs", "recent"]`, `["bookmarks", "add"]`). It does NOT include flag names or flag values. This keeps the field small and predictable, well within the broker's 4 KB `client_info` cap regardless of how the user invokes the CLI.

This shows up in `sessions list` so an operator can tell CLI invocations apart from MCP-shim sessions.

### Errors and exit codes

- `0` — success.
- `1` — runtime error: failed to connect to broker, RPC method error, parse failure on output type, etc.
- `2` — usage error: clap returns this on argument parse failure (standard clap behavior; nothing to add).

Error format:

- **Human mode**: error message written to stderr, formatted as `error: <message>`. Stdout is empty on error. Exit 1.
- **JSON mode**: error written to **stdout** (not stderr) as `{"error": "<message>"}`. No stderr output. Exit 1.

The JSON-mode-to-stdout choice is intentional and goes against the usual UNIX errors-to-stderr convention. Rationale: in `--json` mode the consumer is piping to `jq` or another structured-data tool, and it wants structured output regardless of whether the call succeeded or failed. Splitting success-to-stdout / errors-to-stderr would force every consumer to merge two streams. Distinguish success from failure via the exit code (which `jq` consumers honor via `set -e` or `$?`).

The human-mode behavior is conventional: `set -e; logmon-mcp ...` short-circuits on failure; `if logmon-mcp ...; then ...` works.

### Connection timeout / retry policy

CLI is one-shot — every invocation is a fresh `Broker::connect()` followed by a single RPC call followed by exit. The SDK's defaults (`reconnect_max_attempts: 10`, `reconnect_max_backoff: 30s`, derived `call_timeout` of ~5 minutes) are tuned for long-lived clients and would block a CLI invocation against a missing broker for minutes before giving up. CLI mode overrides:

```rust
Broker::connect()
    .reconnect_max_attempts(0)              // no retry on EOF
    .call_timeout(Duration::from_secs(5))   // 5s ceiling on every RPC + initial connect
    .session_name(session)                  // global --session flag, default "cli"
    .client_info(client_info)
    .open()
    .await?;
```

A broker that's down errors immediately. A broker that's up but slow has 5 seconds. No retry storms, no surprise hangs.

### No auto-start

If `Broker::connect()` fails with `BrokerError::Transport(_)` (socket not present, connection refused), the CLI prints:

```
error: broker not running

Start the broker with one of:
  logmon-broker install-service --scope user   (recommended; daemonized via launchd/systemd)
  logmon-broker                                 (foreground, for debugging)
```

Exit 1. No retry, no auto-spawn. Auto-start is the MCP shim's job (one long-lived process), not the CLI's (per-invocation processes).

### Implementation footprint

| Area | Change |
|---|---|
| `crates/mcp/src/main.rs` | Add `Subcommand` enum to existing `Cli` struct. Dispatch on `command.is_some()`. |
| `crates/mcp/src/cli/mod.rs` (new) | `pub async fn dispatch(broker: Broker, cmd: Subcommand, json: bool)`. |
| `crates/mcp/src/cli/{logs,bookmarks,triggers,filters,traces,spans,sessions}.rs` (new) | One module per group. Each: `Cmd` enum + `pub async fn dispatch(broker, cmd, json)`. |
| `crates/mcp/src/cli/format.rs` (new) | Shared `block(...)`, `table(...)`, `json(...)`, `error(...)` helpers + truncation policy. |
| `crates/mcp/src/cli/connect.rs` (new) | `pub async fn connect(session: Option<String>) -> Result<Broker, ExitError>` — handles transport-failure error formatting. |
| `crates/mcp/Cargo.toml` | Add `comfy-table` for table output (small dep, popular, handles alignment + borders). The SDK and clap are already deps. |
| `crates/mcp/tests/cli_*.rs` (new) | One test file per subcommand group, using existing `test_support` harness. |

### Documentation

- **`skill/logmon.md`**: add a short section near the top noting CLI alternative ("In subagents or sessions where MCP isn't available, the same operations are available as `logmon-mcp <subcommand>` invocations"). Keep MCP tools as the primary surface in slash command instructions; CLI is the fallback.
- **`crates/mcp/README.md`** (new): full subcommand reference + examples.
- **Top-level `README.md`**: mention CLI mode in the install/usage section.

## Out of scope (deferred / YAGNI)

- **Long-running subcommands** (`tail`, `wait`). Push notifications stay MCP-only for now.
- **Custom output templates** (e.g., `--format='{seq} {message}'`). Default text format covers Claude's needs; `--json | jq` covers everything else.
- **Bulk-from-file input** (e.g., `logmon-mcp triggers add --from-file triggers.json`). Add when a workflow demands it.
- **Shell completions.** Clap can generate them; defer until the surface stabilizes.
- **Color in human-readable mode.** Claude reads plain text from Bash; color codes are noise. Detect and disable on non-TTY (which is always for Claude).

## Testing

**Unit:** none in v1; the CLI is a thin SDK adapter, and the SDK is already covered. Format helpers can have small unit tests (truncation thresholds, table alignment) if they grow non-trivial.

**Integration:** one test file per subcommand group at `crates/mcp/tests/cli_*.rs`. Each test:

1. Spawns a test daemon via `logmon_broker_core::test_support::spawn_test_daemon`.
2. Drives the CLI via `Command::new("logmon-mcp")` (the binary built by cargo) with appropriate args.
3. Asserts on stdout/stderr and exit code.

Tests must set the `LOGMON_BROKER_SOCKET` env var so the CLI connects to the test daemon's socket instead of `~/.config/logmon/logmon.sock`.

Coverage: at least one happy-path test per CLI subcommand (so all 25 are exercised), plus error-path tests for:

- Broker not running (no socket).
- Invalid arguments (clap exit 2).
- Invalid filter DSL syntax (broker returns RPC error, CLI exits 1).
- `--json` mode round-trips through `serde_json` cleanly for each result type.

## Open questions / known unknowns

None. All design choices are committed:

- Binary name: `logmon-mcp` (not renamed).
- Subcommand grouping: by namespace (logs/bookmarks/...).
- Output format: human-readable text by default, `--json` flag for machine.
- Session default: named `"cli"`, override with `--session NAME`.
- Auto-start: no.
- Notifications: not in v1.

## Migration / back-compat

Zero migration cost. Default invocation (no subcommand) preserves today's MCP stdio behavior. Existing `claude mcp add logmon -- logmon-mcp` registrations keep working unchanged. The new subcommand surface is purely additive.

The new global `--json` flag and `--session NAME` flag are CLI-only — they're parsed by clap's global-arg machinery and consumed by the dispatch layer. The MCP stdio path ignores them (today's MCP shim already accepts and ignores them, modulo `--session`, which it's used for already).

## Alternatives considered (rejected)

1. **Three separate binaries** (`logmon-broker`, `logmon-mcp`, `logmon-cli`). Rejected because the MCP shim and the CLI share dep trees (both are SDK clients) and combining them keeps install footprint smaller. Single client binary eliminates "did I install the right thing?".

2. **Rename the merged binary to `logmon`** (no `-mcp` suffix). Rejected because it breaks existing `claude mcp add logmon -- logmon-mcp` registrations. Not worth the migration cost. If a cleaner name becomes important, add a `logmon` symlink in a follow-up.

3. **Auto-start the broker on first CLI invocation.** Rejected because each CLI call is short-lived; spawning a broker per call is wasteful and surprising. The MCP shim auto-starts because it's long-lived. CLI errors fast with installation guidance instead.

4. **Flat subcommand naming** (`logmon-mcp get-recent-logs`, `logmon-mcp add-bookmark`, etc.). Rejected in favor of grouped form (`logmon-mcp logs recent`) because clap's grouped help (`logmon-mcp logs --help` shows all log operations) is significantly more discoverable.

5. **Notifications via `tail`-style subcommand** (`logmon-mcp triggers wait --filter '...'`). Deferred. Push notifications are MCP's strong suit; polling from CLI works for many cases. Add the wait subcommand later if a clear use case emerges.
