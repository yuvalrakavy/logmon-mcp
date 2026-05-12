# Your AI Assistant Is Debugging Blind. Give It Logs.

When a human debugs a flaky test, they don't sit down and re-read the source from scratch. They tail a log. They re-run with `RUST_LOG=debug`. They open the trace viewer and squint at which span got fat. *Then* — armed with what actually happened — they go back to the code.

When my AI coding assistant debugs that same flaky test, it does none of those things. It reads source, theorizes, edits, and asks me to run the thing and paste the failure back. The loop is slow because the assistant has no eyes on runtime.

I got tired of being the assistant's `tail -f`. So I built **logmon**, a tiny local broker that gives AI coding assistants the same structured visibility into a running program that a developer has. This is a tour of what's in it and why the design choices matter once an LLM is on the other end of the wire.

## The one-paragraph version

logmon is a single Rust daemon you run on your dev machine. Your app emits structured logs (GELF, UDP or TCP) and/or OpenTelemetry traces (OTLP, gRPC or HTTP), all of which the daemon parks in in-memory ring buffers. AI assistants — Claude Code, Cursor, Windsurf, Copilot, Codex CLI, Gemini CLI, anything that speaks MCP — connect through a thin stdio shim and query that telemetry as tools. Multiple sessions, a CLI, and any Rust process linking the SDK all observe the same stream in parallel. No SaaS, no hosted backend, no agents to deploy.

## Architecture

Four crates, one repo:

- **`logmon-broker`** — long-lived daemon. Owns the GELF and OTLP receivers, the log and span ring buffers, and a JSON-RPC server on a Unix domain socket. Run it as a launchd agent or systemd user unit via `logmon-broker install-service`. Or don't — the shim auto-starts it on first use.
- **`logmon-mcp`** — dual-mode binary. Without arguments it's an MCP stdio server. With a subcommand (`logmon-mcp logs recent --json`) it's a CLI that mirrors the MCP surface 1:1.
- **`logmon-broker-sdk`** — typed Rust client. Test harnesses, dashboards, anything Rust that wants to talk to the broker without going through MCP.
- **`logmon-broker-protocol`** — wire types. Ships `protocol-v1.schema.json` (JSON Schema 2020-12), drift-guarded against the Rust definitions, safe to treat as the contract for codegen in other languages.

The architectural bet: **one daemon, many clients, shared buffer.** Multi-session falls out naturally, CLI and MCP shim are trivial, new transports are just another client.

## The features that actually matter once an LLM is on the other end

### Multi-session, with named sessions that survive

The broker tracks every connected session. A Claude Code window in one terminal and a Cursor window in another both see the same buffer of logs without stepping on each other, because triggers and filters are per-session.

Anonymous sessions get a UUID and disappear on disconnect. **Named sessions** (`logmon-mcp --session my-debug`) persist across disconnects and across daemon restarts; their filters, triggers, and bookmarks live in `~/.config/logmon/state.json`. Notifications queue while disconnected, so an assistant reconnecting after a crash sees what fired in its absence.

### A filter DSL the assistant can actually compose

LLMs are good at composing small, regular DSLs. They're worse at fiddly per-tool argument quirks. So logmon has one filter language that runs everywhere:

```
l>=ERROR                       all errors and above
fa=mqtt, l>=WARN               warnings+ from the mqtt facility
connection refused, h=myapp    substring + host
/panic|unwrap failed/          regex
b>=before, b<=after, l>=warn   warnings between two bookmarks
```

Selectors cover the obvious GELF fields (`m`, `fm`, `h`, `fa`, `fi`, `ln`, `l`) plus any custom field your logger emits. Span filters use a parallel set (`sn`, `sv`, `st`, `sk`, `d>=`, `d<=`). The same string works over MCP, from the CLI, and from the Rust SDK.

### Bookmarks and cursors

A **bookmark** is a named position in the broker's monotonic `seq` stream. `add_bookmark("before-fix")` parks a name at the current moment; later, `get_recent_logs(filter="b>=before-fix, l>=warn")` returns warnings that arrived after it. Bookmarks don't move on their own — you can ask "what happened between *before* and *after*" by dropping two and querying the range.

A **cursor** is the same bookmark used through `c>=` instead of `b>=`. Every read atomically advances the bookmark to the max `seq` returned, so `get_recent_logs(filter="c>=poll, l>=ERROR")` called repeatedly gives you "what's new since I last checked" without threading checkpoint values through your own code. No flag on the bookmark — the operator picks the operation.

Why this matters for an AI: it lets the assistant scope queries to *moments in its own workflow* ("everything between when I made the edit and when the test finished") without destructively clearing logs, and without you copy-pasting sequence numbers between turns.

Cross-session reads work. Cross-session *advances* don't — only the owning session can move its own cursor. Bookmarks auto-evict when both buffers have rolled past their seq.

### Triggers with pre/post windows

A trigger is a filter that runs against every incoming log. When it matches, the broker captures `pre_window` entries before and `post_window` after and pushes a notification to the owning session. The pre-trigger buffer ignores buffer filters, so context around a panic is never truncated.

Two triggers auto-create per session: `l>=ERROR` and `mfm=panic`. Add your own with `oneshot=true` for "wake me up the next time the broken thing happens".

### Logs and traces, correlated

OTLP ingests both. They sit in separate ring buffers but share a global `seq` counter, so the same bookmark works across both. Logs carrying `trace_id`/`span_id` are linked to their spans — `get_trace(trace_id=...)` returns the full span tree *and* the logs emitted under it. The assistant can pivot from "this trace was slow" to "which log lines were emitted during the slow span" without correlating by hand.

### Backpressure resilience

A noisy producer should slow itself down, not take the broker down. Concretely:

- Every receiver uses `try_send` into the pipeline channel — full channel means the entry is dropped at the receiver, not enqueued without bound.
- GELF UDP sets `SO_RCVBUF` to **8 MB**, giving the OS a sizeable burst cushion before datagrams hit the floor.
- OTLP gRPC returns `UNAVAILABLE`, OTLP HTTP returns `429`, when the destination channel is **≥80% full**. OTel exporters retry with backoff on their own.
- Per-source drop counters (`gelf_udp`, `gelf_tcp`, and four OTLP variants) surface in `status.get`, so the assistant can see exactly which receiver is bleeding.

### CLI mode and the SDK

`logmon-mcp` is also a CLI — every command mirrors an MCP tool:

```
logmon-mcp logs recent --json | jq '.logs[] | select(.level=="Error")'
logmon-mcp bookmarks add release-rc1
logmon-mcp status
```

CLI mode defaults to a named session called `cli`, so state persists across invocations. Useful in subagents that don't inherit MCP servers, or when you want pipe-friendly output.

For non-MCP consumers — test harnesses, dashboards, archival workers — `logmon-broker-sdk` provides typed methods, a typed notification stream, a filter builder, and a reconnect state machine that resumes named sessions across daemon restarts. Other languages codegen from the schema.

## A realistic session

Imagine a Rust service whose `/export` endpoint occasionally feels slow. I tell my assistant: *"the /export endpoint feels slow this afternoon, can you figure out why?"*

> **Assistant:** Let me grab the recent traces.
>
> *calls* `get_recent_traces(filter="sn=POST /export", count=10)`
>
> Three of the last ten are over 2 seconds, the rest under 200ms. Looking at the slowest:
>
> *calls* `get_trace_summary(trace_id="3f...")`
>
> 92% of the wall-clock is in a child span called `s3.PutObject`. Bookmarking this so we can compare after a fix.
>
> *calls* `add_bookmark(name="before-fix")`, then `get_slow_spans(min_duration_ms=500, filter="sv=export-service", group_by="name")`
>
> `s3.PutObject` is the only outlier — p95 1800ms, the rest under 50ms. You're constructing a fresh `aws_sdk_s3::Client` per request, which re-resolves credentials each time. Want me to switch it to a shared client?

I say yes, it edits, I run a load test:

> **Assistant:** *calls* `get_recent_traces(filter="sn=POST /export, b>=before-fix", count=20)`
>
> p95 for `POST /export` is now 180ms, no outliers. And checking for new errors:
>
> *calls* `get_recent_logs(filter="c>=before-fix, l>=ERROR", count=50)`
>
> Buffer's clean. Looks good.

No copy-pasting log snippets into the chat. The assistant goes and looks.

## What logmon isn't

- **Not a long-term archive.** Ring buffers, in memory. Default 10,000 logs and 10,000 spans. For 30-day retention, pipe through to something built for that.
- **Not a hosted service.** Local daemon. No SaaS, no telemetry leaves your box.
- **Not a multi-host aggregator.** Each developer runs their own broker on their own machine. For fleet-wide log shipping use Loki, Vector, or one of the dozen things that already do that well.
- **Not Windows-first.** Builds on Windows over a 127.0.0.1 TCP fallback, but launchd/systemd integration, `SO_RCVBUF` tuning, and the UDS path are Unix-shaped.

logmon is deliberately a developer-loop tool. The job is to help your AI assistant see what's happening *right now* on *your* laptop.

## Try it in five minutes

```bash
git clone https://github.com/yuvalrakavy/logmon-mcp
cd logmon-mcp
cargo install --path crates/broker --path crates/mcp
logmon-broker install-service --scope user
```

Then point your assistant at it. For Claude Code:

```bash
claude mcp add logmon --scope user -- logmon-mcp
```

Same idea for Cursor, Windsurf, Copilot, Gemini CLI, Codex CLI — see the README for per-client snippets. Once the broker is running, configure your app to send GELF to `localhost:12201` or OTLP to `localhost:4317`/`4318`, ask your assistant "check the logs", and you're off.

If your app is Rust, `cargo add tracing-init` and a single `TracingInit::builder("myapp").init()` wires both GELF and OTLP up — its defaults match logmon's ports, so there's nothing to configure. ([tracing-init](https://github.com/yuvalrakavy/tracing-init) is a sister crate I maintain for exactly this purpose.)

## Why open-source

I built logmon because I needed it, and at this point I use it every day. It's stable enough that it makes my own loop measurably faster, and small enough that someone reading it can hold the whole thing in their head. Both felt like good reasons to put it in front of more people.

If you build with AI coding assistants and you've ever found yourself manually shoveling log lines into a chat window, I'd love your feedback. The repo lives at **https://github.com/yuvalrakavy/logmon-mcp** — issues, PRs, and "have you considered X" emails all welcome.

Now stop reading and go give your assistant something to look at.
