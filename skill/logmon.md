---
name: logmon
user_invocable: true
description: Use when the user mentions logs, traces, errors, crashes, or performance, or asks what happened at runtime. Use when investigating a flaky test, a slow request, or a panic. Use when the logmon MCP tools (get_recent_logs, get_recent_traces, add_trigger, add_bookmark, …) are available. Skip for static log files on disk, historical archives, or projects with no live telemetry pipeline.
---

# Using the log monitor (logmon)

logmon is a local broker daemon that collects structured logs (GELF over UDP+TCP) and OpenTelemetry traces (OTLP over gRPC+HTTP) from running applications and serves them over a Unix domain socket. You read it via the MCP tools listed below, or via the `logmon-mcp <verb>` CLI.

Use logmon when the user wants to know **what the running program actually did**. Source-level reasoning isn't enough for those questions — open the broker first, then go back to the code.

## When to reach for logmon

Reach for it when any of these are true:

- The user says "logs," "traces," "errors," "panic," "crash," "slow," "timeout," "what happened," "investigate," or "debug at runtime."
- A test just failed and the failure isn't obviously in the test code.
- A user-reported bug describes a behavior, not a code path.
- You're about to insert `println!` / `console.log` / `print` to understand control flow — query logmon first.
- **The user wants to know whether a change made things faster** — "did the cache help," "is this slower than before," "where is the time going." Arm a collector *before* the run, not after. See [Time profiling](#time-profiling).
- You're about to time something by hand — wrapping a block in `Instant::now()`, or eyeballing durations across a few traces. A collector aggregates over the whole run instead.
- **You are starting a session on a project with a live telemetry pipeline** — record the provenance once, before you need it. It costs one call, and every log you look at afterwards carries the context that makes it evidence. See [Provenance](#provenance-domain_data).

## When NOT to reach for logmon

Skip it (and say so) when:

- The user is asking about a `.log` file on disk, or an archived log from yesterday — logmon is in-memory and live only.
- The broker isn't running and the project doesn't ship telemetry. Don't try to invent log lines.
- The question is purely about source structure or types — read code, not logs.

## Tool selection at a glance

| You want to … | Call |
|---|---|
| Find out what is even in the buffer | `list_log_fields` |
| See how records spread along one of those fields | `profile_logs(group_by=…)` |
| See the most recent activity | `get_recent_logs` |
| Find errors / panics | `get_recent_logs(filter="l>=ERROR")` or `…mfm=panic` |
| Investigate a known entry's context | `get_log_context(seq=N)` |
| Find a slow request | `get_slow_spans(min_duration_ms=100)` |
| Drill into one request end-to-end | `get_trace(trace_id=…)` |
| Get the timing breakdown of a trace | `get_trace_summary(trace_id=…)` |
| Compare before/after a code change | `add_bookmark` → make change → query with `b>=name` |
| Measure how long something takes, in aggregate | `add_collector(name=…, filter=…)` → run it → `get_collector` |
| Did a change make it faster? | arm → run → `snapshot_collector` ×3 → change → ×3 → `diff_collectors(a="c@*", b="c@*")` |
| Write up what a measurement showed | `document_collectors(names=[…], question=…)` |
| Profile what already ran | `profile_traces(filter=…, group_by="name")` |
| Stream "what's new since I last checked" | `c>=name` filter (cursor — see below) |
| Get notified when X happens later | `add_trigger(filter=…, pre_window=…, post_window=…)` |
| Record what this run was built from | `update_domain_data([{path:"/Build/commit", value:…}, …])` |
| Check whether the provenance has gone stale | `get_domain_data(validated_before_secs=…)` |
| Preserve an incident before the buffer rolls | `create_case(reason=…, anchor={seq:N}, dir=…)` |
| Know whether a window is missing anything | any `export_logs` reply — read `verdict` |
| See the daemon's health | `get_status` |

## Quick commands (Claude Code slash-command UX)

If the host is Claude Code, the user can type `/logmon <args>`. Execute the matching action immediately; don't echo the menu. The slash-command `count` defaults below (`50`, `10`) intentionally differ from `get_recent_logs`'s underlying default of `100` — pick a sensible size for the action.

- `/logmon` — `get_recent_logs(count=50)`, summarize.
- `/logmon errors` — `get_recent_logs(filter="l>=ERROR")`, summarize.
- `/logmon warnings` — `get_recent_logs(filter="l>=WARN")`, summarize.
- `/logmon recent [count]` — `get_recent_logs(count=<count>)`, default 50.
- `/logmon status` — `get_status`, report.
- `/logmon clear` — `clear_logs`. Warn the user this affects every session.
- `/logmon fixed` — `get_recent_logs(filter="l>=ERROR", count=10)`. If empty, "looks fixed"; else show.
- `/logmon watch <filter>` — `add_filter(filter=<filter>)`.
- `/logmon unwatch` — `get_filters`, then remove each one.
- `/logmon sessions` — `get_sessions`, summarize.
- `/logmon traces` — `get_recent_traces`, summarize.
- `/logmon slow` — `get_slow_spans` with default threshold, summarize bottlenecks.
- `/logmon profile [filter]` — `profile_traces(filter=…, group_by="name")` over what's buffered, summarize where the time went.
- `/logmon collectors` — `list_collectors`, summarize what's armed and what each has matched.
- `/logmon trace <trace_id>` — `get_trace(trace_id=…)`.
- `/logmon <DSL expr>` — if the argument contains `=`, `>=`, `/regex/`, or a known selector (`fa=`, `l>=`, `h=`, `m=`, `sn=`, `sv=`, `d>=`), call `get_recent_logs` (log selectors) or `get_slow_spans` (span selectors) with that filter.
- `/logmon help` — print this list. Do **not** call any tools.

On other MCP hosts (Cursor, Windsurf, Codex, etc.) the user invokes via natural language ("show me errors", "what's slow"); the rest of this document applies unchanged.

## CLI fallback

The same operations are available as `logmon-mcp <subcommand>`. Use the CLI when:

- You're inside a subagent — agents spawned via the `Agent` tool don't inherit MCP servers.
- The MCP connection has dropped mid-session.
- You want to pipe through `jq`, `grep`, or `head`.

**Commands are derived from the broker's RPC method names, not from tool names**, so the mapping is mechanical but it is not the tool name with the words rearranged. The rule: take the method, replace `.` with a space and `_` with `-`.

| Tool | Method | Command |
|---|---|---|
| `list_log_fields` | `logs.fields` | `logmon-mcp logs fields` |
| `profile_logs` | `logs.profile` | `logmon-mcp logs profile` |
| `get_recent_logs` | `logs.recent` | `logmon-mcp logs recent` |
| `add_bookmark` | `bookmarks.add` | `logmon-mcp bookmarks add` |
| `get_slow_spans` | `traces.slow` | `logmon-mcp traces slow` |
| `profile_traces` | `traces.profile` | `logmon-mcp traces profile` |
| `get_filters` | `filters.list` | `logmon-mcp filters list` |
| `get_sessions` | `sessions.list` | `logmon-mcp sessions list` |
| `document_collectors` | `collectors.document` | `logmon-mcp collectors document` |
| `create_case` | `cases.create` | `logmon-mcp cases create` |
| `update_domain_data` | `domain_data.update` | `logmon-mcp domain-data update` |

Note the shape: the *group* is the noun (`traces`, `collectors`, `cases`), so `get_slow_spans` is **not** `spans slow` and `profile_traces` is **not** `collectors profile`. When unsure, **ask the binary rather than guessing** — `--help` is built from the same manifest the daemon just served, so it is always current where a table can lag:

```bash
logmon-mcp --help                  # every group
logmon-mcp collectors --help       # one group's verbs
logmon-mcp cases create --help     # one command's arguments and accepted values
```

Global flags — `--session NAME`, `--domain NAME`, `--json` — go **before** the command; after it, a flag belongs to the tool (`logmon-mcp collectors edit --domain …` re-pins a collector; `logmon-mcp --domain t3 logs recent` scopes the query). CLI invocations default to a named session called `"cli"` so state persists across calls.

Output arrives **already rendered** for most reads; `--json` opts out and gives the raw result with no rendered field to strip.

## Architecture (one-paragraph version)

`logmon-broker` is a long-running daemon that ingests GELF (UDP/TCP on `12201`) and OTLP (gRPC `4317`, HTTP `4318`), stores logs and spans in in-memory ring buffers, correlates them by `trace_id`, and serves multiple clients over `~/.config/logmon/logmon.sock` via JSON-RPC 2.0. `logmon-mcp` is the thin MCP shim — one per editor session, all sharing the same broker — and it holds no knowledge of the daemon: the tools you are holding, their parameters, the CLI's commands **and this document itself** were all assembled at startup from the broker's `tools.manifest`. Each session owns its triggers, filters, bookmarks and collectors; named sessions persist across reconnects and daemon restarts.

## Available tools

### Logs

- **`list_log_fields(filter?, top_values?, min_coverage_pct?)`** — **reach for this first when you do not know what is in the buffer.** Every field present, with coverage, distinct count, top values and type. It walks the whole ring, not the newest N.

  Why first: any grouping or filtering needs a **field name**, and a name that does not exist returns an empty result rather than an error — so guessing costs you a silent wrong answer. This is the map.

  Four things it tells you that nothing else will:
  - **Filter with the row's `selector`, NOT its field name.** They differ, and the difference is silent. GELF strips the `_` prefix, so `_file` becomes an additional field named `file` sitting *beside* the built-in `file` — two different fields, reached by `file` and `fi` respectively. Both may appear as separate rows. Typing the name where a selector was needed matches nothing and reports no error.
  - **A row at 0% coverage means the field EXISTS and is never populated.** The normal case for the top-level GELF built-ins (`fa`, `fi`, `ln`) when an emitter sends everything as underscore-prefixed extras. Use the spelling that has coverage.
  - **`kind`** (`string` / `integer` / `float` / `number` / `bool` / `mixed`) tells you which fields a numeric aggregation could sum. It is stated from the schema for built-ins, so a never-populated `line` still reports `integer`.
  - **`selector: (none)`** means **no log filter reaches this field at all.** That is `trace_id` and `span_id`: the parser lifts them out of `additional_fields`, so `trace_id=…` in a filter matches nothing silently. Use `get_recent_logs(trace_id=…)` instead.

  `names_capped` means there were more distinct field names than the cap and some rows are missing. `truncated` needs a lower bound (`b>=`) to mean anything — with no bound, read `buffer_oldest_seq` / `lost_below` to see whether the ring wrapped.

- **`profile_logs(group_by?, group_keys?, filter?, top_n?)`** — **how the records DISTRIBUTE along one field.** `list_log_fields` says which dimensions exist; this says what the population looks like along the one you pick. Counts, a per-level breakdown, seq/time bounds, and a verbatim exemplar at *each end* of every group.

  These two are one workflow. Run `list_log_fields` first, take a row, then name it here:

  ```
  logmon-mcp logs fields                                  # what is in here?
  logmon-mcp logs profile --group-by field --group-keys target --filter "l>=Warn"
  ```

  ```
  log profile by `field` [target] — 11 matched of 101 scanned, 2 groups

    key                    count  share  Error   Warn  seqs      exemplar
    store::rhai                7  63.6%      7         91-97     run script failed
                                                                 run script failed (retry 4)
    store::mqtt                4  36.4%             4  98-101    MQTT backpressure
  ```

  **How to name the axis.** A built-in goes straight into `group_by` — `level`, `message`, `host`, `facility`, `file`, `line`, `trace_id`, `span_id`. An emitter field needs `group_by="field"` plus `group_keys` naming it, and the name is the `field` value from a `list_log_fields` row. Repeat `--group-keys` for a tuple, joined with ` / ` in the key. Passing `group_keys` on a built-in axis is an error rather than an answer to a question you did not ask.

  **`__absent__` is a normal row, and it is usually the biggest one.** Most axes are absent from over 90% of records — profiling by `kind` on a typical buffer puts 86% of them there. It exists so the counts still account for every matched record. It sorts last and does **not** consume `top_n`, so `top_n=20` buys twenty real values.

  **`__overflow__` is a different fact** — the cardinality cap folded keys — and the two are never merged. If you see it, `cardinality_capped` is set and that row aggregates an arbitrary, arrival-ordered set.

  **Read `groups_total`.** The rows sum to `matched` only when it is at most `top_n`; otherwise you are looking at a sample and the header says `top N of M`.

  **A `suppressed` entry means the axis you named appears on NO matched record.** That is almost always a spelling problem, not an empty buffer — and for a built-in it usually means the emitter sends that name as an underscore-prefixed extra instead. The remedy says which call works:

  > `line` did not appear in any of the 9347 matched records… an additional field named `line` covers 99.97% — use `group_by="field", group_keys=["line"]`

  **Both exemplars are worth reading.** They print on one line when the group did not change and two when it did, so a second line means the group's shape moved across the window — for a recurring error, `run script failed` then `run script failed (retry 4)` is most of the diagnosis.

  Counts only for now: no sums or averages over field values. Walks the whole ring, not the newest N.

  Cursor qualifiers (`c>=`) are refused by **both** of these reads: a cursor advances on read, so two identical calls would describe different populations. Use `b>=` for a repeatable window. The refusal happens before resolution, so it leaves no bookmark behind.
- **`get_recent_logs(count?, filter?, trace_id?)`** — newest-first by default; **oldest-first** when the filter contains `c>=` (cursor). Default `count=50`.
- **`get_log_context(seq, before?, after?)`** — logs around a specific entry. Use this when you have a `seq` from another query.
- **`export_logs(path, count?, filter?, from_seq?, to_seq?, format?)`** — write matching logs to a file (`json` or `text`). `from_seq`/`to_seq` are **inclusive** and compose with a bookmark bound. The reply carries a **`verdict`** — see below.
- **`export_spans(from_seq?, to_seq?, count?, filter?)`** — the same inclusive range over the span ring, for pairing spans with the logs of one window. Its retention is reported separately: the two stores share a seq axis but evict independently.
- **`clear_logs()`** — clear the in-memory buffer. **Shared across all sessions.** Prefer bookmarks for "see only what happens next" — see below.

#### `verdict` — how much of a window logmon can vouch for

Storage is **conditional**: when any session bound to a domain holds a filter, only
matching entries are kept. So *"nothing appeared before the error"* is ambiguous between
**absence of cause** and **absence of recording**, and only the first is a conclusion.
`export_logs` answers that directly:

| `verdict` | Means |
|---|---|
| `complete` | the window lay wholly inside one unfiltered epoch, nothing was evicted, nothing was capped |
| `filtered` | a session filter was narrowing the store over part of it — `narrowed_by` names **which filters, over which seqs** |
| `evicted` | the ring dropped part of the window; `evicted_before_window` bounds how much |
| `cannot_verify` | no claim is possible: the store is empty, the window predates this daemon run, or the read was capped |

**`cannot_verify` is the default when the field is absent**, so a missing verdict never
reads as a clean bill of health. And note the filter that narrowed your
window may belong to **another session, possibly a disconnected one** — its filters go on
shaping storage until the TTL sweep retires it. That is precisely the case you cannot see
by asking what filters *you* hold.

### Filters (per-session, shape what gets stored)

- **`get_filters` / `add_filter(filter, description?)` / `edit_filter(id, …)` / `remove_filter(id)`** — when any filter exists, only matching records are stored. OR semantics across filters within a session; the union across all sessions is what the broker keeps.

### Triggers (per-session, push notifications)

- **`get_triggers` / `add_trigger(filter, pre_window?, post_window?, notify_context?, oneshot?, description?)` / `edit_trigger(id, …)` / `remove_trigger(id)`**.
- Defaults on every new session: `l>=ERROR` and `mfm=panic`.
- `pre_window` captures **unfiltered** context before the match (flight recorder). `post_window` captures after. `notify_context` is how many of the pre-window entries ride along in the notification.
- `oneshot=true` removes the trigger after the first match — useful for "tell me the next time this happens."
- A trigger is **debounced by its own `post_window`**: inside the window opened by its last match it won't fire again, so a burst yields one capture. The debounce is per trigger and never silences a different one — arming `kind=deadlock` alongside the noisy built-in `l>=ERROR` is safe, the busy trigger cannot starve the rare one. Use `post_window=0` to count every match — but only for a LOW-RATE signal. A firing entry costs ~200 µs against ~0.6 µs for a normal one (store scan + context clones), `post_window=0` also gives up aftermath capture, and notification delivery is a bounded channel that drops silently client-side when it can't keep up. On a bursty signal, keep a window.
- Because of that debounce, `match_count` is "capture count", not "matching entries seen". Don't read it as an event tally on a bursty signal.
- **The debounce is for LOG triggers only.** A trigger filtering on span selectors runs on a separate path: it fires on every matching span and is never debounced, so its `post_remaining` stays `0`. Its `match_count` IS counted, so "has this ever fired?" is answerable for span triggers too — but don't expect `post_remaining` to explain a quiet one.

### Bookmarks (named seq positions)

- **`add_bookmark(name, start_seq?, description?, replace?)` / `list_bookmarks(session?)` / `remove_bookmark(name)` / `clear_bookmarks(session?)`**.
- A bookmark is just a `(session, name) → seq` mapping. Two operators use it: `b>=` (pure read) and `c>=` (read-and-advance).

### Sessions

- **`get_sessions` / `drop_session(name)`** — list connected sessions; remove a named session and its state. Named sessions persist across reconnects; a *disconnected* one is disposed automatically after the broker's session TTL (default 24 h).
- **`rename_session(name)`** — rename the current session in place; all state (domain binding, triggers, filters, bookmarks) survives. Use it to claim a meaningful identity (convention: `<Project>-Main-<uuid8>` for a main/home session, `<Project>-tN-<branch>` for a worktree lane). An "already connected" error means another LIVE client holds that name — stop and surface it to the user rather than picking a variant; a *disconnected* holder is displaced automatically (`displaced_stale_holder: true`).

### Traces (OTLP)

- **`get_recent_traces(count?, filter?)`** — index page: trace id, root span, total duration, error flag.
- **`get_trace(trace_id, include_logs?, filter?)`** — full span tree + linked logs. `include_logs` defaults to **`true`** — only pass `false` if you specifically want just the spans.
- **`get_trace_summary(trace_id)`** — timing breakdown of the root span's direct children, with percentages.
- **`get_slow_spans(min_duration_ms?, count?, filter?, group_by?)`** — slow individual spans, or aggregates when `group_by="name"`. Defaults: `min_duration_ms=100`, `count=20`. In the grouped arm the statistics cover **every** matching span of that name, and `min_duration_ms` only decides which names are shown — so `avg_ms` far below the floor beside a high `max_ms` means "usually fast, with a tail", which is the useful reading.
- **`get_span_context(seq, before?, after?)`** — spans surrounding a given span.
- **`get_trace_logs(trace_id, filter?)`** — only the logs linked to one trace.

### Time profiling

Measuring the effect of a change means comparing two runs. A collector accumulates timings for every span matching a filter, so you arm it once and read totals rather than eyeballing individual traces.

Collectors need a **named** session (`--session NAME`, or `session.start` with a name). An anonymous session's identity is a UUID that is never presented again, so anything it armed would be unreachable the moment it disconnected. For a one-off measurement with no session, use `profile_traces`.

- **`add_collector(name, filter, level?, group_keys?, description?)`** — arm it. `level`: `scalar` (counts and totals), `timing` (adds percentiles, wall union, warm-up exclusion), `tree` (adds self time, nesting and call paths — the default). `group_keys` splits the numbers by span attribute, which is how you run both arms of an A/B in one pass: `group_keys=["cache.enabled"]`. Always give a `description` — it comes back with every read.
- **`get_collector(name, snapshot?, group_by?, skip_warmup_ms?, top_n?)`** — read it. `group_by`: `name`, `group`, `trace`, `path`. `snapshot` reads a recorded run instead of the live window — and a recorded run is served from what it stored, so `group_by` and `skip_warmup_ms` cannot apply to one and will say so. **Shape the read before you snapshot, not after.**
- **`list_collectors()`** — what this session has armed, and how much each has matched. Check this before arming: only about four default-sized collectors fit in the daemon-wide budget.
- **`snapshot_collector(name, label?, description?, meta?, reset?, projections?, per_name?, per_group?)`** — record the current window as a named run and start the next one. **This is the between-runs move**, not `reset_collector`: it keeps the run. Pass a `description` and, when you have one, a `meta` like `{"commit": "abc123"}`.
  - `reset` defaults **true** — ending one run and starting the next is the usual intent. Pass `false` to record without zeroing.
  - `projections`, `per_name`, `per_group` all default **true** and are **computed now or never**: the samples behind them are not retained, so a run recorded with one of these false can never grow the breakdown later. Leave them alone unless you know you do not want the detail.
- **`get_collector_history(name, limit?, merge?)`** — the recorded runs, oldest first, each with the definition it was taken under. `merge=true` also combines them and reports the run-to-run spread — which is what tells you whether a gap between two runs is real or noise. A single run reports that spread as *unknown*, never zero.
- **`edit_collector(name, …)`** — change an armed collector. `group_keys` is capped at 8 (group by one attribute — that is the case this is built for). A structural edit is refused if it would exceed the daemon-wide sample reservation, and a refused edit changes nothing. Editing only `description` costs nothing; editing `filter`, `level`, `group_keys`, `max_sample_bytes` or `domain` **discards the live window** (snapshots are never touched). Use it to re-pin a collector orphaned by a restart, or to drop `tree` → `timing` for 2.5× the records when the sample budget runs out.
- **`reset_collector(name)`** — zero it and **discard** the run. Prefer `snapshot_collector` unless you genuinely want it gone.
- **`remove_collector(name)`** — unarm it and hand the budget back. Do this when you're done; a collector left armed keeps costing ingest time and reserved memory.
- **`diff_collectors(a, b, group_by?, …)`** — subtract two runs and get what moved. **This is the payoff**; everything above measures. An *arm* is `"<collector>"` (the live window), `"<collector>@<label>"` (one recorded run), or `"<collector>@*"` (every recorded run merged). **Prefer `@*` on both sides** — with single runs there is no spread, so every threshold comes back `unknown` and nothing in the result can be called a result.
- **`document_collectors(names, format?, question?, finding?, path?)`** — write it up: what moved, what to do next, and every caveat attached to the number it qualifies. The first name is the baseline. Pass `question` when you generate it and `finding` on a second call once you have read it — regeneration is free and lossless, which is why nothing is stored. `format: "folded"` gives collapsed stacks for a flame graph, one arm at a time, `tree` level only. `path` is resolved by **your** client, not the daemon (same as `export_logs`), and writes the document plus its sidecar to disk instead of returning them.
- **`profile_traces(filter?, …)`** — the same projection, over the span buffer instead of a collector. See below for which to reach for.

**`profile_traces` or `get_collector`?** They return the same shape and answer different questions, so the choice is made *before* the run, not after:

|  | `profile_traces` | a collector |
|---|---|---|
| Measures | spans already in the buffer | spans arriving from the moment you arm it |
| Set up | nothing | `add_collector` before the run |
| Bounded by | the buffer's ring (10 000 spans, oldest evicted silently) | its own retention budget, and it says when truncated |
| Survives a restart | no | definition and snapshots do |
| Can compare runs | no | `snapshot_collector` + `diff_collectors` |

**Reach for `profile_traces` when the run already happened** and you want to know where the time went — a one-off, no session needed, nothing to clean up. It is the right tool for "something was slow just now."

**Arm a collector when you are about to measure something**, especially when you will measure it more than once. It cannot be applied retroactively: a collector only sees spans that arrive after it is armed, which is the one mistake worth avoiding. If you are about to change code and want to know whether it got faster, arm first.

A useful pattern: `profile_traces` to find *what* is slow, then arm a collector on that filter to track it across changes.

**Reading the result.** `exact`, `estimated` and `sampled` are not three views of one number:

- `exact` — every matched span, for the collector's whole life. Trust `count`, `total_ms`, `avg_ms`.
- `estimated` — percentiles from a sketch over the same population, accurate to ±1%.
- `sampled` — exact over the records retained, which is everything only while `complete` is `true`. Self time, wall union and call paths live here.

**At small n, read `sampled.durations_ms` and stop eyeballing.** When a collector matches once per run — three runs, three records — the percentiles are order statistics of three numbers and say nothing. `durations_ms` lists every retained duration **in arrival order** (`[0]` is the first, *not* the smallest) whenever the sample is complete and holds at most 50; `stddev_ms` gives the spread from two records up. Both cover the same population as the percentiles, after any warm-up cut, and both are recorded into a snapshot — so a run captured under these conditions keeps its own raw durations for good. Read them back with `get_collector(name, snapshot=label)`: the runs *listing* from `get_collector_history` deliberately drops the duration arrays, since one bounded list per run times fifty runs is not a bound. Treat `stddev_ms` as a description of the spread, not a significance test: separating two three-run means properly takes a difference of roughly 2.3 standard deviations, not one.

**`skip_warmup_ms` reports its own effect.** `excluded_by_warmup` says how many spans it removed; that plus `sampled.sample_count` accounts for everything retained. It is **absent** — never `0` — when no count could be produced, either because no cut was asked for or because none could be positioned. Note it cuts the *sample* tier only: whenever a cut actually runs, `exact`, `estimated`, and the `name` and `group` breakdowns are all withheld, because they come off accumulators written at ingest that have no window. Group by `trace` or `path` for a breakdown under a cut — those need level `tree` — or reset the collector after warm-up so the window needs no cut at all.

Any field that could not be computed is `null` with an entry in `suppressed` saying why and, usually, what to change. `null` and `0` mean different things — `self_ms: null` with `nested_matches: 0` means the filter matched no nested spans, not that no time was spent. The same rule governs `groups_total`: absent when no grouping happened, including a grouping that was asked for and refused.

**`matched: 0` is ambiguous on its own, so read `zeroed_by` beside it.** Absent means nothing has emptied the window — no traffic yet. Otherwise it names what did: `snapshot` (the run was kept), `reset` (it was discarded), `edit` (the definition changed under it), or `daemon_restart`.

**If `snapshot_collector` returns `durable: false`**, the run is in the response and in the daemon's memory but was not written to disk, so it will not survive a restart. Read the `durability_warning`, and if you need that run, copy the numbers out now.

Two ways to run an A/B:

1. **One pass, `group_keys`** — emit a span attribute naming the arm, run both interleaved, read `group_by="group"`. Immune to drift between runs.
2. **Two passes, `snapshot_collector`** — arm, run A, `snapshot_collector(label="before")`, change the code, run B, `snapshot_collector(label="after")`, then `get_collector_history(merge=true)`. Use when the arm cannot be an attribute. Both runs are kept, each with its own definition and description.

#### `group_keys`, end to end

This is the one worth learning, and it is easy to skip because `group_keys: []` appears in every response whether or not you ever set it. **It replaces hand-rolled per-case counters**: one collector splits the numbers by any span attribute, and it needs no change to the code being measured beyond emitting that attribute.

Arm it with the attribute you want to split by:

```
add_collector(name="cache-ab", filter="sn=Lookup", group_keys=["cache.enabled"],
              description="does the read-through cache pay for itself")
```

Read it back with `group_by="group"`:

```
get_collector(name="cache-ab", group_by="group")
```

Each row is one value of the attribute, ranked by total time:

```json
"grouped_by": "group",
"groups_total": 2,
"groups": [
  { "key": "false",
    "exact": { "count": 500, "total_ms": 4210.0, "avg_ms": 8.42, "min_ms": 6.1, "max_ms": 31.7 },
    "estimated": { "p50_ms": 8.1, "p95_ms": 14.2 } },
  { "key": "true",
    "exact": { "count": 500, "total_ms": 1180.0, "avg_ms": 2.36, "min_ms": 1.9, "max_ms": 9.4 },
    "estimated": { "p50_ms": 2.2, "p95_ms": 4.0 } }
]
```

Points worth knowing before you rely on it:

- **`key` is the attribute *value* alone**, not `attribute=value` — `"false"`, not `"cache.enabled=false"`. The attribute name is the one you declared; the rows tell you which value. With several `group_keys` the values join with `" / "`, in declaration order, so `["region","cache.enabled"]` yields `"eu / true"`. Values are read as strings, so booleans and numbers work.
- **Rows rank by total time descending**, so the arm that cost the most is first — which is usually the one you are trying to fix.
- **`groups_total` is the count before `top_n` truncation** (default 20). `groups_total: 2` with two rows means you are seeing everything; `groups_total: 900` with 20 rows means you are seeing the top slice.
- **Group rows come off the exact tier**, so they carry `exact` and `estimated` but no `sampled` block — no self time, no call paths, per row.
- **A warm-up cut withholds them.** `skip_warmup_ms` windows the sample tier only, and these rows are unwindowed, so they are suppressed rather than served alongside windowed headline figures. Group by `trace` or `path` under a cut, or reset the collector after warm-up instead.
- **Cardinality is capped.** Unbounded attributes (a user id, a request id) fold into `__overflow__` and set `cardinality_capped`. Group by something with a handful of values.

**Repeat before you conclude.** Two runs differing by 5% tell you nothing until you know the run-to-run spread. Take three snapshots of the *same* configuration first, read the `floor` from `get_collector_history(merge=true)`, and treat differences below it as noise. A single run reports the spread as unknown, which is the honest answer, not zero.

### Comparing, and what a diff refuses to do

`diff_collectors` subtracts; most of its behaviour is the cases where it refuses to. Three severities:

- **Marked** — it proceeds and says what differs. Arms at different levels (compared at the lower one, with nesting evidence carried across anyway), filters that differ only in spelling, one truncated arm.
- **Blocked, with a flag that permits it** — the arms matched different populations (`allow_mismatch`), one lost spans and the other did not (`allow_lossy`), both hit the sample budget (`allow_truncated`). The error names the flag.
- **Refused outright** — mismatched sketch layouts (the subtraction would be arithmetic on two different scales, so there is nothing to permit), and a `@*` arm whose recorded runs carry **different definitions**: a structural edit keeps history, so a collector's history can legitimately span configurations, and summing them would report the spread across configurations as scheduling variance. The refusal names both runs; compare them individually by label, or re-record under one definition.

**Every row carries the threshold used to suppress it, and that is the threshold that was applied** — there is no second, stricter bound doing the striking. A bracketed `[Δ]` is below its printed floor: the number is real, what is missing is any basis for calling it a change.

**`err/Δ` on an estimated percentile row is the error bar as a percentage of the delta.** At 100% the bar is as wide as the delta and the *sign* of the change is not established. A 1% change carries ±199%; below roughly a 2% relative change an estimated delta is not resolvable at all. This is a property of the sketch — repeating the run will not improve it, which is what makes it a different floor from the run-to-run one.

**A count row and a duration row get different floors.** A suite with a fixed iteration count has near-zero count variance while its timings vary by percent, so the two are never thresholded against each other.

### Guarding a run

- **`add_collector(…, threshold={metric, op, value, window_ms, group?})`** — a rolling guard. `metric`: `count`, `total_ms`, `avg_ms`, `error_count`, `error_rate_pct`. `op`: `gt`, `gte`, `lt`, `lte`. Read the verdict back from `list_collectors` or `get_collector`.

**The window advances on span arrival, not on a clock.** That is what makes an idle collector free, and it has one consequence worth knowing before you rely on it: **with no traffic a breached threshold neither fires nor clears.** It is a load-time guard, not a liveness check — a `lt` threshold detects a drop *while traffic continues* and does nothing at all if traffic stops. Every report carries a note saying so, so a stuck `breached: true` on a finished run is not a bug.

**Percentiles cannot be thresholds.** A rolling percentile needs a duration sketch per bucket, and per-collector memory is bounded on purpose. Use `avg_ms` for the guard and `get_collector` for the real percentiles.

`fires` counts clear-to-breached transitions, not evaluations — a threshold breached for a whole run has fired once. `last_value` is **absent** until something has been evaluated: zero is a value `count` legitimately holds, and an `lt` guard would read it as a breach. Changing a threshold zeroes the live window, like any other structural edit; pass `threshold: null` to remove one. `effective_window_ms` in the report is the window as evaluated — the declared width rounded up to a whole number of ring buckets, never narrower than asked for. A reset or kept snapshot clears the guard's window and verdict along with the data.

**Renaming a session keeps its collectors** — they move with it. If the rename displaces a disconnected session that held the same name, that session's collectors are cleared rather than inherited, so you never read another conversation's measurements.

**Across a restart.** Collectors and their history survive; the live window does not. A restored collector reports `zeroed_by: "daemon_restart"`, so `matched: 0` is distinguishable from "no traffic yet". One armed on an ephemeral domain comes back `orphaned` — that domain is not re-created — and the result says to re-pin it with `edit_collector`.

### Status

- **`get_status()`** — uptime, receivers, store stats, **`receiver_drops`** counts, **`trace_ingest`**, plus **`current_domain`** (your bound domain), **`active_filters`** (what's narrowing you), and **`receiver_liveness`** (per-listener last-received — pinpoints *which* port is silent). Check the drop counts when investigating "missing logs."

  It also reports **`broker_version`** and **`broker_tools`** — see below.

**The tools you hold came from the broker itself — and so did this document.** The shim keeps no tool list and no skill of its own: on startup it calls `tools.manifest` and builds its MCP router, its CLI, and its `instructions` from that one reply. So your tool list *is* the running broker's surface, `broker_tools` agrees with it by construction, and **if you are reading this as MCP server instructions, the text and the tools shipped together and cannot disagree.**

**So a tool named in this document that you do not hold means you are not reading the broker's copy.** You have it from the Claude Code plugin or a local skills directory — a file on disk, which can be any age. Check `broker_version` against the document you are holding before concluding a capability is missing.

The other possibility is a daemon that predates the document legitimately: the broker is long-lived and does not pick up a rebuild until restarted, so this is what a fresh build nobody restarted into looks like:

```
The logmon broker answering here doesn't serve create_case — it's running an
older build than this checkout. Rebuild and restart it:
`cargo install --path crates/broker --locked`, restart the broker service,
then restart this MCP server.
```

Read `broker_version` the first time you call `get_status`, and reach for `broker_tools` when a tool you expected isn't there. **Don't hand-roll around a missing capability without saying so.** `snapshot_collector` and `diff_collectors` are how a before/after comparison is done at all; substituting hand-computed ratios silently is exactly the failure these fields exist to prevent.

**If you are reading this and have NO logmon tools at all**, the shim refused to start — so this copy necessarily came from the Claude Code plugin or a local skills directory rather than from the MCP server. The shim requires `tools.manifest` and will not serve a partial surface. Same fix, same order: rebuild the broker, restart it, then restart the client. The `logmon-mcp` CLI is not a fallback here — it fails the same way, with `could not read the tool manifest`.

**A skill change ships with the broker, not the shim.** Since the daemon serves this document, correcting it means `cargo install --path crates/broker --locked` and a broker restart; reinstalling `logmon-mcp` alone changes nothing about the text you are reading.

**`trace_ingest` — loss on the OTLP trace transports, before any collector saw a span.** Non-zero on any of the three means every span-derived number you report from elsewhere is a **lower bound**, `matched` included.

**Do not add `trace_ingest.dropped` to the `receiver_drops` trace fields — it IS those fields.** `dropped` is exactly `receiver_drops.otlp_http_traces + otlp_grpc_traces`, the same counters read a second time so the three trace figures read as one block. Summing them double-counts. `shed_batches` and `malformed_dropped` are the genuinely new numbers.

Also: `shed_batches` counts **request bodies**, not spans — the bodies were refused with 429/UNAVAILABLE before being parsed, so how many spans were in them is unknowable. And the standing "bump `buffer_size`" remedy applies only to channel-full drops: a `malformed_dropped` span was refused for cause (an unusable trace id), and no buffer size changes that.

### Domains

Domains are isolated broker instances — each has its own log/span buffers, receivers (GELF/OTLP ports), triggers, and filters. Use them to keep unrelated log streams from interleaving (e.g. one per dev-server or test run). The `default` domain is the always-on anchor; a session stays on `default` until it switches.

- **`list_domains()`** — live domains with ports, source, log/span counts, `bound_sessions` (which sessions are bound to each domain; disconnected holders suffixed), and **liveness**: `last_log_received_at` / `last_span_received_at`, `idle_secs`, `stale`. A `last_*_received_at` of `null` means nothing has shipped to that domain yet — the "is my stack actually reaching this domain, or did I misconfigure the port?" check.
- **`create_domain(name, gelf_port?, otlp_grpc_port?, otlp_http_port?, ...)`** — create (or idempotently ensure) an ephemeral domain. Omitted ports auto-allocate; `0` disables that receiver.
- **`use_domain(name)`** — bind this session; subsequent queries and trigger notifications target it until you switch again.
- **`clear_domain()`** — dispose the bound domain's logs + spans (keeps it alive; seq stays monotonic).
- **`delete_domain(name)`** — delete a domain and tear down its receivers (refuses `default`).

The MCP shim can bind a domain **at connect** via the `LOGMON_DOMAIN` env var (or `--domain`), re-applied on every reconnect **when paired with a named `--session`** — set both once per worktree/project so every session auto-scopes to its track, durably. (An anonymous session can't resume a restart: it fails loud, never a silent revert to `default`; a missing domain is a loud error.) In CLI mode there is no `use` verb — pass `--domain NAME` (or `LOGMON_DOMAIN`) to scope one invocation (e.g. `logmon-mcp --domain t3 logs recent`); it resets to `default` when both are absent. Durable domains can be declared in `config.json`. **Lifecycle:** an ephemeral domain lives until you `delete_domain` it or the broker restarts — there is no idle auto-reaping (so a stopped stack's logs stay queryable), and re-creating with the same name + ports is an idempotent no-op.

### Provenance (`domain_data`)

A small key/value registry per domain, recording what was true of the project while the
logs were being produced. **Logs without provenance are a log dump; logs with it are
evidence.** Six months on, "the checkout worker stalled" is worth nothing without "under
which commit, in which build, doing what".

- **`update_domain_data(entries)`** — `entries` is a list of `{path, value?, ttl?}`.
  **Value present** → set it. **Value absent** → *validate*: confirm what is already there
  without changing it. Never creates from a key alone.
- **`get_domain_data(prefix?, validated_before_secs?)`** — read it back, each key with its
  age and any expiry verdict. `validated_before_secs` is the "what has gone stale" query.
- **`remove_domain_data(patterns)`** — prefix patterns, matched on segment boundaries
  (`/Versions` removes `/Versions/*`; `/Ver` removes nothing). **There is no undo.**

#### The keys to record

**Core — record these three, always.** A case document missing them cannot be acted on.

| Key | What it is |
|---|---|
| `/Build/commit` | `git rev-parse HEAD`. The only exact identity of the code — a version string is a label someone maintains, a SHA is what actually ran |
| `/Build/profile` | `debug` or `release`. logmon is a timing instrument and the two differ by an order of magnitude, so comparing durations across profiles is not a comparison |
| `/Action` | what you are doing, in prose: `"full test suite"`, `"checkout smoke, 20 iterations"`. Without it a reader has logs and no scenario |

**Contextual — record when they apply.**

| Key | Answers |
|---|---|
| `/Versions/<component>` | "which release of *which part*" — plural, because the failing one is rarely the one you upgraded |
| `/Build/branch` | "was this even mainline" |
| `/Env/host`, `/Env/os`, `/Env/container` | "only on CI?" — the first question about anything intermittent |
| `/Data/dataset`, `/Data/seed` | **`/Data/seed` is the highest-value key you can record.** It turns "fails 1 in 20" into a reproduction, and it is the one most often skipped because writing it down feels like bookkeeping at the moment it costs nothing |
| `/case-name` | the filename prefix for this project's case documents — a slug (`checkout`, `ht-server`), not prose |

`/logmon/*` is logmon's own and is rejected if you write to it. Anything outside these
namespaces is yours.

#### When to record

**At session start**, one call with **what you actually re-read** — the commit from
`git rev-parse`, versions from the lockfile, the profile from the build you just ran.
Anything you did *not* re-derive, send **key-only** to validate rather than restating its
value: a `ttl` runs from the last confirmation, so restating a value you only assumed buys
it a freshness it has not earned.

```
update_domain_data(entries: [
  {path: "/Build/commit",  value: "<git rev-parse HEAD>"},
  {path: "/Build/profile", value: "release"},
  {path: "/Action",        value: "checkout smoke, 20 iterations", ttl: "30m"},
  {path: "/Env/host"},                        # key-only: confirm, do not restate
])
```

**When the answer changes** — a deploy, a branch switch, a different scenario. `/Action`
in particular is stale within minutes of switching tasks, and **a stale `/Action` is worse
than an absent one, because it reads as fact.**

**Create a domain per project.** Everything that does not is sharing one registry with
every other project on this machine.

#### Reading the outcomes

One per entry, in the order you sent them. `created` is news. `updated` means the value
changed — worth noticing if you did not expect it to. `validated` is confirmation.
`scoped` means the key carried an `@` and went to a case document instead of the registry
(see below). `rejected` carries a `reason`. `unknown` means a key-only entry found nothing, with a
`cause`: `never_set` (the registry is there, that key is not) or `undetermined` (no
registry at all, and nothing establishes why — which may mean it was lost). If you see
`undetermined` where you expected a populated registry, check `/logmon/first_seen` and
`/logmon/incarnation` before re-setting everything: re-setting resets each key's
`created_at`, which turns months-old confirmed facts into fresh-looking ones.

#### `ttl` — how long a value stays believable

Optional, per key, as `30s` / `5m` / `2h` / `7d` / `4w`. `ttl: false` clears one.

Set it where you know the answer rots on a clock: `/Action` in minutes, `/Versions/*` in
days. **Leave it off and the reader gets an age and no verdict** — which is correct and
deliberate: without a stated lifetime, logmon will not tell anyone a value is current.

### Case documents (`create_case`)

Everything in the buffer is **in memory and rolling**. The moment you decide something is
worth understanding later, capture it — a restart, a `clear_logs` by another session, or
simply enough traffic, and the evidence is gone with no error anywhere.

```
create_case(
  reason:  "checkout hangs at 20/20, reproducibly",   # required
  anchor:  {seq: 41022},                              # or {bookmark: …} / {trace_id: …}
  dir:     "/abs/path/to/docs/cases",                 # required, ABSOLUTE
  prefix:  "checkout-hang",                           # optional
  before:  350, after: 350,                           # stored RECORDS either side
  data:    [{path: "/Env/host", value: "ci-7"},       # → the registry
            {path: "@/Data/seed", value: "8814"}],    # → this document only
)
```

**Three files, sharing one stem:**

```
checkout-hang-260731-021530.md               ← read this to triage
checkout-hang-260731-021530.logdata.jsonl    ← the log records
checkout-hang-260731-021530.spandata.jsonl   ← the spans
```

The document is what you read to decide **whether this is your bug**; the logdata is the
evidence you consult once you have decided it is. The document leads with what could
**not** be captured — the `verdict`, the span line, which core provenance keys are missing
— before anything it qualifies.

**The things that bite:**

- **`dir` must be absolute.** The broker is a service; a relative path would resolve
  against *its* working directory, so it is rejected rather than resolved.
- **The anchor is tagged.** `{seq}` / `{bookmark}` / `{trace_id}`, exactly one. Not one
  string logmon guesses at — a bookmark named `12345` and a seq are indistinguishable, and
  since the anchor's message becomes the headline, a wrong anchor is a wrong document.
  A `trace_id` matching several entries anchors on the earliest and says so.
- **An unresolvable anchor is an error**, not a document with an empty headline.
- **`before`/`after` count stored RECORDS, not seqs** — one counter feeds both the log and
  span stores, so a 200-*seq* range holds an unpredictable number of logs. Default 350
  each, capped at 5000.
- **`@` scopes a key to this capture.** It reaches the document, keeps its sigil there, and
  **never enters the registry** — otherwise the next case on this domain would silently
  inherit the last one's seed. Use it for what is true of *this incident*: a seed, an
  iteration number, a hypothesis. Use a plain key for what is true of the *domain*.
- **`data` is `update_domain_data`**, applied before the registry copy is rendered — so a
  key you supply appears in *this* document rather than landing one document late. The one
  exception is **`/case-name`, which names the next capture, not this one**: the filename
  is claimed before anything durable happens, so a `/case-name` sent in the same call shows
  up in this document's registry copy while the stem still comes from `prefix`, or from
  whatever `/case-name` already held. Set it in its own call first if you want it to bite.

**Capture before you investigate, not after.** The document records both instants and
renders the gap, so a fact you record twenty minutes later is visibly a fact about
twenty minutes later. That is honest, and it is also weaker evidence than the same fact
recorded at the time.

## What a reply looks like

Most reads come back **already rendered** — a block list of records, or a padded
markdown table — rather than as a JSON envelope. That is the daemon's doing, not
the shim's, and you do not ask for it.

**Read it as the answer.** The rendering is not a summary: it carries every key
on the result except the record array itself. When a log read comes back

```
(no logs)
count=0  scanned=4000  truncated=false
the filter matched 0 of 4000 scanned records — data is flowing, so the filter is what to check
```

that last line is the finding. `(no logs)` alone would have read as a quiet
system.

**Three things in a rendered reply that are easy to skim past and shouldn't be:**

- **`verdict`** — how much of the window the daemon can vouch for. `complete`,
  `filtered`, `evicted`, `cannot_verify`. Absent means `cannot_verify`.
- **`cursor_advanced_to`** — the read **moved your cursor**. The next call
  returning nothing is expected, not a bug.
- **`… N more record(s)`** — the list was cut at 50 records or 16 KB. What you
  are looking at is not all of it; narrow the filter or lower `count`.

**Mutations still return JSON** — `add_filter`, `clear_logs`, `add_bookmark` and
the rest. They are small and flat, and the fields are what you need to reference
them later. Nothing is being withheld: a method with no renderer returns its
result unchanged.

## Filter DSL

Comma-separated qualifiers, AND-ed within a filter. Multiple filters on a session OR together.

| Pattern | Meaning |
|---|---|
| `text` | case-insensitive substring against all fields |
| `/regex/` | regex (add `/i` for case-insensitive) |
| `selector=pattern` | match against a specific field |
| `l>=L` / `l<=L` / `l=L` | level filter (`ERROR`, `WARN`, `INFO`, `DEBUG`, `TRACE`) |
| `b>=name` / `b<=name` | match records strictly after / before the bookmark's seq |
| `c>=name` | cursor: same as `b>=` but advances the bookmark to the highest returned seq |
| `"quoted"` | literal — use when the value contains commas or `=` |
| `ALL` / `NONE` | match everything / nothing |

Only `>=` and `<=` are accepted for `b`, `d`, and `l`; only `>=` for `c` (`c<=` is rejected by the parser). The level filter additionally allows `l=` for an exact match.

> **Off-by-one note:** despite the `>=` / `<=` syntax, `b>=name` matches records with seq **strictly greater** than the bookmark's seq, and `b<=name` strictly less. The bookmark's own record is never included on either side. Same applies to `c>=`.

**Log selectors:** `m` (message), `fm` (full_message), `mfm` (either), `h` (host), `fa` (facility), `fi` (file), `ln` (line), `l` (level). Any other selector (e.g. `user_id`, `request_id`) is treated as a GELF additional field — drop the leading underscore that GELF uses on the wire (`user_id=42`, not `_user_id=42`).

**Span selectors:** `sn` (span name), `sv` (service), `st` (status: `ok|error|unset`), `sk` (kind: `server|client|producer|consumer|internal`), `d>=` / `d<=` (duration ms).

Log selectors and span selectors **cannot mix in the same filter** — they target different stores. Log filters apply to `get_recent_logs`, `get_log_context`, `export_logs`, `get_trace_logs`; span filters apply to `get_recent_traces`, `get_slow_spans`, `get_span_context`, and to the `filter` argument of `get_trace`.

Examples:

```
l>=ERROR                      all errors and worse
fa=mqtt, l>=WARN              warnings+ from the mqtt facility
connection refused, h=myapp   substring match + host
/panic|unwrap failed/         regex for panics
m="POST /users, 200"          literal — needed because of the comma
user_id=42, l>=WARN           custom GELF field, no underscore prefix
sn=query_database, d>=100     spans named query_database taking ≥100 ms
sv=auth, st=error             error spans from the auth service
b>=before, b<=after           records strictly between two bookmarks
c>=test-run-abc               records since last poll, advances the cursor
```

## Bookmarks: when and how

Use a bookmark instead of `clear_logs` when you want a clear before/after boundary but don't want to lose history.

Reach for them when:

- You're about to start a flaky operation and want to inspect just that range later.
- You're comparing two attempts at the same operation — bookmark each attempt's start.
- You'd otherwise call `clear_logs` to "see only what happens next."

```
add_bookmark("before-deploy")
# … run the operation …
add_bookmark("after-deploy")
get_recent_logs(filter="b>=before-deploy, b<=after-deploy, l>=warn")
get_recent_traces(filter="b>=before-deploy, b<=after-deploy, d>=100")
```

Naming: bookmarks are stored as `{session}/{name}`. Bare `before` in a query resolves to your own session; `other/before` reaches into another session's bookmarks (pure-read across sessions is fine; cross-session **advance** with `c>=` is rejected).

`b>=`, `b<=`, and `c>=` are query-only — rejected by `add_filter` and `add_trigger`.

## Cursors: "what's new since I last checked"

A cursor is a bookmark used with `c>=` instead of `b>=`. Every read with `c>=` atomically advances the bookmark to the highest seq returned, so the next read sees only what's new. No checkpoint state to thread through your own code.

```
# First call — if the bookmark doesn't exist, it's auto-created at seq=0,
# so this returns everything currently in the buffer matching the filter.
get_recent_logs(filter="c>=test-run, l>=ERROR", count=500)

# Subsequent calls — only the delta since the previous call.
get_recent_logs(filter="c>=test-run, l>=ERROR", count=500)
```

Results are returned **oldest-first** when `c>=` is present, so a paginated drain stays monotonic.

`c>=` is allowed in `get_recent_logs`, `export_logs`, and `get_trace_logs`. Rejected in `get_log_context`, `get_recent_traces`, `get_trace_summary`, `get_slow_spans`, `get_trace`, and `get_span_context` — their results are anchor-driven or aggregated, not seq-streamable. Only one `c>=` per filter.

To pre-position a cursor at "now" (so the first read returns only future records), call `add_bookmark("name")` first — the default `start_seq` is the current seq counter.

## Triggers vs bookmarks: which one?

| Use a bookmark when… | Use a trigger when… |
|---|---|
| You know roughly when the interesting thing happens and want to query that range later. | You don't know when it'll happen and want to be told. |
| You're doing a manual before/after comparison. | You want pre/post context captured automatically around the event. |
| You're polling regularly (cursor). | You want push notifications. |

A bookmark is passive metadata; a trigger is an active watcher with windowed context capture.

## Worked patterns

### Pattern: debug a specific module

```
result = add_filter(filter="fa=<module>")   # returns the new filter's id
# … ask the user to reproduce …
get_recent_logs(count=100)                  # examine
remove_filter(id=result.id)                  # restore full capture when done
```

(`add_filter`, `add_trigger`, and `add_bookmark` accept either positional or named arguments; this skill uses named for clarity.)

### Pattern: before/after a change

```
add_bookmark("before-change")
# … make the change, restart the service, etc …
get_recent_logs(filter="b>=before-change, l>=warn")
get_recent_traces(filter="b>=before-change, d>=100")
```

### Pattern: catch the next occurrence of a rare event

```
add_trigger(
    filter   = "mfm=connection refused, fa=db",
    pre_window  = 500,
    post_window = 200,
    notify_context = 10,
    oneshot = true,
)
# Continue working. When it fires, you'll be notified with surrounding context.
```

### Pattern: investigate a slow request end-to-end

```
get_slow_spans(min_duration_ms=200, group_by="name")
# ↑ pick a span name that stands out, then find a specific trace:
get_recent_traces(filter="sn=<that-name>, d>=200", count=5)
# ↑ note a trace_id, then:
get_trace_summary(trace_id="<id>")        # where did the time go?
get_trace(trace_id="<id>")                 # full span tree + logs interleaved
```

### Pattern: log + trace correlation in one shot

This pattern only works when the application is exporting OTel traces **and** emitting logs (GELF or otherwise) with a `trace_id` field — e.g. `tracing-init`'s GELF layer, OTel auto-instrumented HTTP middleware, etc. If logs don't carry a trace id, fall back to timestamp-based correlation.

When a user reports "this request was broken" and gives you a `trace_id`:

```
get_trace(trace_id="<id>")     # include_logs defaults to true
# Returns the span tree AND every log line linked to that trace.
# Now you have timing AND log context in one response.
```

When they only give you a timestamp or symptom, find the trace first:

```
get_recent_logs(filter="l>=ERROR, h=<host>")     # find the error log
# Note the trace_id field on the matching entry, then:
get_trace(trace_id="<that_id>")
```

### Pattern: paginated drain of a long burst

```
loop:
    r = get_recent_logs(filter="c>=drain, l>=warn", count=500)
    if r.logs is empty: break
    process(r.logs)
# Cursor auto-advances each call; oldest-first ordering keeps it monotonic.
```

### Pattern: zoom in on the context around an error

```
r = get_recent_logs(filter="l>=ERROR", count=5)   # find the error(s)
# Each entry carries a `seq`. Pick the one you care about:
get_log_context(seq=r.logs[0].seq, before=20, after=10)
# Returns 20 entries before and 10 after, regardless of level/filter —
# the full unfiltered run-up to and recovery from the error.
```

### Pattern: preserve an incident you will want in six months

The buffer is in memory and rolling. Capture at the moment you decide it matters, not
after you have finished investigating.

```
r = get_recent_logs(filter="l>=ERROR", count=5)
create_case(
  reason: "checkout hangs at 20/20 under the new lock ordering",
  anchor: {seq: r.logs[0].seq},
  dir:    "/abs/path/to/docs/cases",
  prefix: "checkout-hang",
  data:   [{path: "/Build/commit", value: "<git rev-parse HEAD>"},
           {path: "@/Data/seed",   value: "8814"}],
)
```

Then **read the returned `verdict` before you conclude anything.** `complete` means the
window holds everything that reached the daemon over those seqs. `filtered` means a
session filter — possibly another session's, possibly a disconnected one's — was
narrowing what got stored, and `narrowed_by` names it: over that range, "nothing appeared
before the stall" is not a supported conclusion, and no later read can recover what was
never stored. Clear the filter and reproduce.

### Pattern: comparing two test attempts

```
add_bookmark("attempt-1-start")
# … run test attempt 1 …
add_bookmark("attempt-1-end")
add_bookmark("attempt-2-start")
# … run test attempt 2 …
add_bookmark("attempt-2-end")

get_recent_logs(filter="b>=attempt-1-start, b<=attempt-1-end")  # 1's logs
get_recent_logs(filter="b>=attempt-2-start, b<=attempt-2-end")  # 2's logs
```

## When things look wrong

### "I'm getting zero logs back"

In order:

1. `get_status` — is the broker even running? Is uptime sensible? Are receivers listed?
2. Does the application emit telemetry yet? Many projects send GELF only after a feature flag flips. Ask the user to trigger an action that should produce a log.
3. Is a filter narrowing the buffer? `get_filters` — if filters exist, the buffer only stores matches. Remove them or widen.
4. Did someone (you, another session) call `clear_logs`? The buffer is shared.
5. Check `receiver_drops` on `get_status`. Non-zero means the receivers couldn't keep up — the user's app is over-producing; suggest bumping `buffer_size` in `~/.config/logmon/config.json`.

### "My cursor returned a huge unexpected flood"

A cursor was idle long enough that its seq fell off the ring buffer. The broker auto-recreated it at `seq=0`, so it returned the entire current buffer. A WARN-level log entry was emitted by the broker noting the rollover. Either poll the cursor more often or raise `buffer_size`.

### "A trigger isn't firing"

1. Triggers evaluate **every** incoming log, regardless of filters. So a filter isn't the cause.
2. A trigger is debounced only by its OWN `post_window` — a sibling firing never suppresses it. But within that window it won't re-fire, so a burst gives you one capture, not one per entry; set `post_window=0` to count every match — on a low-rate signal only; see the cost note under Triggers above.
3. CLI mode invocations can't receive trigger fires — the CLI process exits before any log can match. Use the MCP shim or the SDK for that.
4. Confirm the trigger exists AND is armed: `get_triggers` — `post_remaining > 0` means it is inside its own debounce window right now, so it is suppressed rather than broken.
5. Verify the filter actually matches incoming logs by trying the same filter in `get_recent_logs`.

### "The broker isn't running"

If the MCP shim is connected, the broker is running by definition. If you're hitting the CLI and seeing "broker not running":

```
logmon-broker status                       # check
logmon-broker install-service --scope user # install as launchd/systemd, start
```

Don't suggest editing `~/.config/logmon/state.json` or `daemon.pid` by hand unless the user explicitly asks — those are managed.

### "I cleared logs and now I have no context"

`clear_logs` is shared across all sessions and destructive. There's no undo. Going forward, prefer `add_bookmark("checkpoint")` + `b>=checkpoint` for scoped queries — same outcome, no data loss, and other sessions aren't affected.

## Multi-session behavior

- **Shared (within a domain):** the log/span ring buffers, the receivers, `clear_logs`.
- **Per-session:** triggers, filters, bookmarks, collectors, queued notifications.
- **Domains:** a session can bind to an isolated **domain** — its own buffers, receivers, triggers, and filters — via `use_domain` (or the `--domain` CLI flag); data never crosses domains. See Domains above.
- **Anonymous sessions** (no `--session` flag): cleaned up on disconnect. They **cannot own a collector** — an anonymous identity is a UUID that never returns, so anything armed would be unreachable after a disconnect, and `add_collector` refuses with that reason.
- **Named sessions** (`logmon-mcp --session NAME`): persist across disconnect; trigger fires queue while disconnected and replay on reconnect; state survives daemon restart via `state.json`, and collectors via `~/.config/logmon/collectors/`. A *disconnected* named session is swept once it passes the session TTL (default 24 h); a connected one never expires.

Filters are unioned across sessions (the broker stores anything any session's filters match), so adding a narrow filter in your session doesn't hide records from another session — but if every session has a narrow filter, only the union is stored.

## SDK consumers (non-MCP)

For test harnesses, dashboards, archival workers, anything that isn't an MCP client, point them at the typed Rust SDK at `crates/sdk` (`logmon-broker-sdk`). The SDK speaks the same JSON-RPC protocol, builds filter strings without manual escaping, and includes a reconnect state machine for named sessions. Cross-language clients can codegen from `crates/protocol/protocol-v1.schema.json` (drift-guarded by `cargo xtask verify-schema`).

If a user mentions building a custom integration, redirect them to `crates/sdk/README.md` rather than wrapping the MCP shim.
