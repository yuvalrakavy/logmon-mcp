# Your AI Assistant Is Debugging Blind. Give It Logs.

When a human debugs a flaky test, they don't sit down and re-read the source from scratch. They tail a log. They re-run with `RUST_LOG=debug`. They open the trace viewer and squint at which span got fat. *Then* — armed with what actually happened — they go back to the code.

When my AI coding assistant debugs that same flaky test, it does none of those things. It reads source, theorizes, edits, and asks me to run the thing and paste the failure back. The loop is slow because the assistant has no eyes on runtime.

I got tired of being the assistant's `tail -f`. So I built **logmon**, a tiny local broker that gives AI coding assistants the same structured visibility into a running program that a developer has. This is a tour of what's in it and why the design choices matter once an LLM is on the other end of the wire.

## The one-paragraph version

logmon is a single Rust daemon you run on your dev machine. Your app emits structured logs (GELF, UDP or TCP) and/or OpenTelemetry traces (OTLP, gRPC or HTTP), all of which the daemon parks in in-memory ring buffers. AI assistants — Claude Code, Cursor, Windsurf, Copilot, Codex CLI, Gemini CLI, anything that speaks MCP — connect through a thin stdio shim and query that telemetry as 47 tools. Multiple sessions, a CLI, and any Rust process linking the SDK all observe the same stream in parallel. No SaaS, no hosted backend, no agents to deploy.

## Architecture

Four crates, one repo:

- **`logmon-broker`** — long-lived daemon. Owns the GELF and OTLP receivers, the log and span ring buffers, and a JSON-RPC server on a Unix domain socket. Run it as a launchd agent or systemd user unit via `logmon-broker install-service`. Or don't — the shim auto-starts it on first use.
- **`logmon-mcp`** — dual-mode binary. Without arguments it's an MCP stdio server. With a subcommand (`logmon-mcp logs recent --json`) it's a CLI that mirrors the MCP surface 1:1. Both are assembled at startup from what the broker declares, so a tool added to the daemon becomes an MCP tool and a CLI command with no reinstall. The shim ships with no tool list, no parameter schemas, no CLI verbs — and no documentation. All four arrive in one handshake.
- **`logmon-broker-sdk`** — typed Rust client. Test harnesses, dashboards, anything Rust that wants to talk to the broker without going through MCP.
- **`logmon-broker-protocol`** — wire types. Ships `protocol-v1.schema.json` (JSON Schema 2020-12), drift-guarded against the Rust definitions, safe to treat as the contract for codegen in other languages.

The architectural bet: **one daemon, many clients, shared buffer.** Multi-session falls out naturally, CLI and MCP shim are trivial, new transports are just another client.

A smaller bet that took longer to notice: **an assistant needs a map before it can measure.** Every query tool asks you to name a field — group by `kind`, filter on `service` — and a name that does not exist comes back empty rather than wrong, which is worse, because empty reads as an answer. An assistant arriving at an unfamiliar buffer does not know whether this service tags its errors with `kind`, or `ty`, or nothing at all. It guesses, gets nothing, and reports that nothing was found. So there is a tool whose whole job is to answer *what is in here* — every field, how much of the buffer carries it, its distinct values, its type. Unglamorous, and the one that makes the rest usable. The tell that it was missing: writing the tools that came after it, we kept hand-rolling it in a shell to find out which fields to write about.

The bet has a corollary that took a while to follow all the way down: if the daemon is the single source of truth, the shim should hold *nothing*. Tools went first, then their schemas, then the CLI's verbs. The last holdout was the agent-facing guide — a markdown file compiled into the shim, which meant a documentation fix required rebuilding and reinstalling a binary that had nothing to do with the change, and which could describe a broker it was not talking to. Now it travels in the same reply as the tool list. The document and the surface it documents ship together, so they cannot drift apart; the class of bug where your assistant confidently explains a capability the running daemon does not have is gone, rather than guarded against.

---

The tool started as "let the assistant read the logs." It grew two more jobs, and they turned out to matter more than the first one. What follows is organised that way: **see it**, **measure it**, **keep it**.

## Part one: see it

### Multi-session, with named sessions that survive

The broker tracks every connected session. A Claude Code window in one terminal and a Cursor window in another both see the same buffer of logs without stepping on each other, because triggers and filters are per-session.

Anonymous sessions get a UUID and disappear on disconnect. **Named sessions** (`logmon-mcp --session my-debug`) persist across disconnects and across daemon restarts; their filters, triggers, and bookmarks live in `~/.config/logmon/state.json`. Notifications queue while disconnected, so an assistant reconnecting after a crash sees what fired in its absence.

**Domains** partition the data itself. One per project or test run, each with its own ports, its own ring buffers, and its own seq axis — so two projects on one machine never read each other's telemetry, and clearing one leaves the other alone.

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

### Triggers, and logs correlated with traces

A trigger is a filter that runs against every incoming log. When it matches, the broker captures `pre_window` entries before and `post_window` after and pushes a notification to the owning session. The pre-trigger buffer ignores buffer filters, so context around a panic is never truncated. Two triggers auto-create per session: `l>=ERROR` and a panic regex. Add your own with `oneshot=true` for "wake me up the next time the broken thing happens".

Logs and spans sit in separate ring buffers but share one `seq` counter, so the same bookmark works across both. Logs carrying `trace_id`/`span_id` are linked to their spans — `get_trace(trace_id=...)` returns the full span tree *and* the logs emitted under it. The assistant pivots from "this trace was slow" to "which log lines were emitted during the slow span" without correlating by hand.

## Part two: measure it

Traces tell you what one request did. They're bad at telling you whether a *change* helped, because answering that means aggregating many runs and knowing whether the difference is bigger than the noise. Assistants do this badly by default: they eyeball three numbers and declare victory.

So logmon has a measuring instrument. You arm a **collector** over a span filter *before* the run — `add_collector(name="lookup", filter="sn=Lookup", level="tree")` — and it accumulates while the workload executes. Then you read exact totals, percentiles, self time (duration minus the union of matched children, so concurrent children don't double-count), and call paths.

**Runs are kept, not just read.** `snapshot_collector(label="before")` closes a window and starts the next; `diff_collectors("lookup@before", "lookup@after")` subtracts them and reports what moved — **and refuses when the arms aren't comparable**, naming the flag that would permit it anyway. Merge several runs of one configuration and it reports the run-to-run spread, so a 5% difference gets called noise or signal instead of guessed at. A single run reports that spread as *unknown*, which is the honest answer and not zero.

You can also arm a **threshold** on a collector — `avg_ms` over a rolling window — and let the broker tell you when the workload crosses it, rather than polling.

## Part three: keep it

Everything above lives in a ring buffer. The moment you decide something is worth understanding later, it is already on a countdown: a restart, a `clear_logs` from another session, or simply enough traffic, and the evidence is gone with no error anywhere.

**`create_case` freezes a window to disk.** Three files sharing one stem:

```
checkout-hang-260731-021530.md               ← read this to triage
checkout-hang-260731-021530.logdata.jsonl    ← the log records
checkout-hang-260731-021530.spandata.jsonl   ← the spans
```

The markdown document is what you read to decide *whether this is your bug*; the JSONL is the evidence you consult once you've decided it is. The document leads with what could **not** be captured — before anything it qualifies.

### The registry: what was running when this happened

A log window without provenance is a puzzle. logmon keeps a small per-domain key-value store — `update_domain_data` — and the case document renders it as of the capture:

| key | what it is |
|---|---|
| `/Build/commit` | `git rev-parse HEAD`. The only exact identity of the code — a version string is a label someone maintains, a SHA is what actually ran |
| `/Build/profile` | `debug` or `release`. logmon is a timing instrument and the two differ by an order of magnitude |
| `/Action` | what you were doing, in prose. Without it a reader has logs and no scenario |

Each key carries the age of its last confirmation, so a stale value reads as stale rather than as fact. A key prefixed `@` is asserted about *that capture alone* and never enters the registry — a random seed belongs to one incident, not to the project.

### This is where "not an archive" stopped being true

The old version of this article said logmon was *"not a long-term archive — ring buffers, in memory."* Half of that is still right: the **live buffer** is not an archive and never will be. But a case is a deliberate, durable artifact that outlives the ring, the daemon, and the machine's uptime. It is meant to be committed next to the code it describes.

That is a smaller claim than "log retention", and a more useful one. You don't want thirty days of everything. You want the twenty minutes around the thing that broke, with enough context to know what produced it, still readable in six months.

## The thread running through all three: an instrument that admits what it can't tell you

This is the design constraint that shaped more of logmon than any other, and it exists **because** an LLM is reading the output. A human who gets a suspicious number squints at it. An assistant uses it.

**Collectors decline to report figures they can't compute.** Every suppressed field comes back `null` with a reason and a remedy: *"no matched span has a matched parent, so self time would equal total time by construction — broaden the filter so nested spans are matched too."* An instrument that quietly returns a plausible-but-meaningless number is worse than one that refuses. This turned out to be the feature users cite as the reason they trust the rest of the output.

**They report what they did to the data, not just the result.** `skip_warmup_ms` says how many spans were excluded; a grouped read says how many groups existed before `top_n` truncated the list. "Skipped nothing" and "skipped half the data" are otherwise indistinguishable downstream.

**A breakdown accounts for its own population, or it is a chart that lies.** When you ask logmon how log records distribute along some field — `kind`, say — the honest answer on a real buffer is that 86% of them don't carry that field at all. Most tools drop those records and draw you a clean pie chart of the remaining 14%, which is a picture of a population you never asked about. logmon gives absence a row of its own, `__absent__`, and a second reserved row, `__overflow__`, for keys the cardinality cap folded together. They're deliberately not the same row: *"this record lacked the field"* and *"this record's value was one of too many"* are different facts, and merging them makes the denominator lie in a way nothing downstream can detect.

The two rows sort last and don't consume your `top_n`, so asking for twenty rows still buys twenty real values — and if the axis you named turns out to be absent from *every* record, that isn't answered with a lone `__absent__` row that reads like a result. It's announced, with the fix: *"`line` did not appear in any of the 9347 matched records… an additional field named `line` covers 99.97% — use `group_by="field", group_keys=["line"]`."* That particular collision is the one GELF hands you for free, and it's the reason the map (`list_log_fields`) exists one call before the measurement.

**They know a small sample from a real one.** At three runs, percentiles are order statistics of three numbers and say nothing — so the sampled block also returns raw durations in arrival order plus a Bessel-corrected standard deviation. That turns "are these two arms actually separated?" from a judgement by eye into arithmetic. And it's honest about the limit: separating two three-run means properly takes a difference of roughly 2.3 standard deviations, not one.

**And every range read carries a verdict.** This one is subtler, and it is the reason cases are trustworthy. Storage in logmon is *conditional* — a filter held by any session narrows what the daemon keeps, and the session holding it may be one you can't see. So a quiet buffer has two completely different explanations: nothing happened, or nothing was recorded. Every read answers which:

| verdict | means |
|---|---|
| `complete` | everything that reached the daemon over this range was stored |
| `filtered` | a session filter was narrowing the store over part of it — and **which filters, over which seqs** |
| `evicted` | the ring had already dropped part of the window |
| `cannot_verify` | no claim is possible: the store is empty, the window predates this daemon run, or the read was cut short |

`cannot_verify` is the **default** when the field is absent, so a reply from a broker too old to send one never reads as a clean bill of health. And a case document puts the verdict at the top, before the evidence it qualifies — because the most expensive thing an archive can do is look complete when it isn't.

## Backpressure resilience

A noisy producer should slow itself down, not take the broker down. Concretely:

- Every receiver uses `try_send` into the pipeline channel — a full channel means the entry is dropped at the receiver, not enqueued without bound.
- GELF UDP sets `SO_RCVBUF` to **8 MB**, giving the OS a sizeable burst cushion before datagrams hit the floor.
- OTLP gRPC returns `UNAVAILABLE`, OTLP HTTP returns `429`, when the destination channel is **≥80% full**. OTel exporters retry with backoff on their own.
- Per-source drop counters surface in `status.get`, so the assistant can see exactly which receiver is bleeding.

## CLI mode and the SDK

`logmon-mcp` is also a CLI — every command mirrors an MCP tool:

```
logmon-mcp logs recent --json | jq '.logs[] | select(.level=="Error")'
logmon-mcp bookmarks add release-rc1
logmon-mcp status
```

CLI mode defaults to a named session called `cli`, so state persists across invocations. Useful in subagents that don't inherit MCP servers, or when you want pipe-friendly output.

For non-MCP consumers — test harnesses, dashboards, archival workers — `logmon-broker-sdk` provides typed methods, a typed notification stream, a filter builder, and a reconnect state machine that resumes named sessions across daemon restarts. Other languages codegen from the schema.

## A realistic session

A Rust service whose `/export` endpoint occasionally feels slow. I tell my assistant: *"the /export endpoint feels slow this afternoon, can you figure out why?"*

> **Assistant:** Let me record what we're running first, so whatever I find is attributable.
>
> *calls* `update_domain_data(entries=[{path:"/Build/commit", value:"9f2a1c4"}, {path:"/Build/profile", value:"release"}, {path:"/Action", value:"investigating slow /export"}])`
>
> *calls* `get_recent_traces(filter="sn=POST /export", count=10)`
>
> Three of the last ten are over 2 seconds, the rest under 200ms. 92% of the wall-clock on the slowest is in a child span called `s3.PutObject`.
>
> Before this rolls out of the buffer, I'm freezing it.
>
> *calls* `create_case(reason="POST /export p95 blowout, 3 of 10 over 2s", anchor={trace_id:"3f…"}, dir="/repo/docs/cases")`
>
> Captured — verdict `complete`, so nothing was filtered or evicted over that window. Now let me measure rather than eyeball.
>
> *calls* `add_collector(name="put", filter="sn=s3.PutObject", level="tree")` … *load test runs* … `snapshot_collector(name="put", label="before")`
>
> 40 calls, avg 1,840ms, p95 2,100ms, run-to-run spread 6%. You're constructing a fresh `aws_sdk_s3::Client` per request, which re-resolves credentials each time. Want me to switch it to a shared client?

I say yes, it edits, I re-run the load test:

> **Assistant:** *calls* `snapshot_collector(name="put", label="after")`, then `diff_collectors("put@before", "put@after")`
>
> avg 1,840ms → 47ms. The arms are comparable — same filter, same level, both release builds — and the change is far outside the 6% run-to-run spread, so that's signal, not noise.
>
> The case document from before the fix is still in `docs/cases/`, with the commit that produced it recorded in the front matter.

No copy-pasting log snippets into the chat. The assistant goes and looks, measures, and leaves a record that survives the buffer.

## What logmon isn't

- **Not a log-retention system.** The live buffers are in memory — 10,000 logs and 10,000 spans by default. Cases are deliberate, hand-sized archives of a specific window, not a firehose sink. For 30-day retention of everything, pipe through to something built for that.
- **Not a hosted service.** Local daemon. No SaaS, no telemetry leaves your box.
- **Not a multi-host aggregator.** Each developer runs their own broker on their own machine. For fleet-wide log shipping use Loki, Vector, or one of the dozen things that already do that well.
- **Not Windows-first.** Builds on Windows over a 127.0.0.1 TCP fallback, but launchd/systemd integration, `SO_RCVBUF` tuning, and the UDS path are Unix-shaped.

logmon is deliberately a developer-loop tool. The job is to help your AI assistant see what's happening *right now* on *your* laptop — and, when it matters, to keep an honest record of it.

## Try it in five minutes

```bash
git clone https://github.com/yuvalrakavy/logmon-mcp
cd logmon-mcp
cargo install --path crates/broker --locked
cargo install --path crates/mcp --locked
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
