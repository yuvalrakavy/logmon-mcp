# Monitor a parallel test run

A dashboard can discover busy lanes and show progress without access to the runner’s filesystem. A lane is a logmon domain. For an interactive investigation, look at recent failures and the longest-running executions first. A case can become ready while other tests continue.

```sh
logmon-mcp --json domains list
logmon-mcp --json --domain ci status get
logmon-mcp --json --domain ci test-log status
logmon-mcp --json --domain ci test-log list
logmon-mcp --json --domain ci test-log status --run-id build-42
```

Use the read-only MCP tools `test_log_status` and `test_log_list` for the same run responses. Supply the domain and, for a specific run, its ID. Polling reads the current broker snapshot. For a multi-lane page, read the domain list once per poll and query detail only for active lanes. Polling every two to five seconds is a reasonable starting point.

## Idle, busy and finished lanes

Version 1 has three useful responses for an otherwise quiet lane.

| Request | Result |
|---|---|
| Current status on an idle domain | `{"schema_version":1,"domain":"ci","test_log":null}` |
| List on a domain without retained history | `{"schema_version":1,"domain":"ci","runs":[]}` |
| Per-domain fields in domain discovery or status | `"test_log_schema_version":1,"test_log":null` |

When a lane is busy, its projection has `run_id`, `state` and `in_flight`. An open run with zero in flight is between batches. A closing run has stopped accepting executions and is draining work. After settlement, current status is null. Select the run by ID to read its `closed` or `sealed` snapshot. An old last-event timestamp is useful for spotting an abandoned open run. Use the explicit seal operation to close it.

The broker retains the last **ten settled runs per domain**, plus the current run. Settlement order determines eviction. List displays retained runs newest start first, with aggregate counts and run fields. Restarting the broker or removing the domain clears query history. The journal and case files follow the runner’s retention policy.

## Read progress

- `executions` counts attempts and chains. An execution with several dependent steps contributes one. Set `--planned-executions N` on `test-log begin` when the runner knows the total in these units. An unknown count is null, and zero is an explicitly empty plan. A run can settle below its plan when tests are cut.
- `by_verdict` includes OK, FAIL, TIMEOUT, ABORTED and NO_VERDICT, even for zero counts. Its sum equals `ended`. A broker deadline without a runner result contributes NO_VERDICT. A runner-reported timeout contributes TIMEOUT. A subsequently accepted end can replace a missing verdict without increasing ended.
- `longest_in_flight` shows up to three executions by oldest start, including their execution ID, trace ID when known, age and overdue time. `last_completion_at` is null until the first execution ends.
- `recent_failures` shows the latest 20 runner failures, newest first, with the assertion and case outcome when available. A truncation flag indicates that older failures exist. The runner can supply a rerun command. Case paths are relative to the JSONL directory. A remote page must map that directory to an accessible location.
- The `cases` counters describe capture decisions for individual executions. Several executions may share one report. Use `distinct_cases_written` and the grouped file list to count or navigate actual cases. Use `summaries.pending` to see how many ended executions still await their final summary. `replayed_ends` counts newly applied replayed ends. Unconfirmed local deliveries are known only to the runner.
- `case_status` tracks creation as `pending`, `ready`, or `error`. `case_size` describes the evidence as `full`, `minimal`, or `bare`. A bare case reports `capture_queue_full` or `capture_pool_exhausted` in `case_detail`. An OK rerun or slow pass can still report `skipped_rerun_limit`, `skipped_slow_limit`, `skipped_queue_full`, or `skipped_pool_exhausted`. A queued slow-pass snapshot can be marked `evicted_for_priority` when a higher-priority capture takes its slot.
- `cases.max_failures` is the current full-case limit, and `cases.minimal_by_failure_limit` counts minimal cases admitted under it. The legacy `refused_by_max_failures` counter stays zero. See the [test-log guide](../README.md#nightly-test-logs).
- Times use UTC and durations use milliseconds. The journal health field reports known write errors. For offline readers, consume complete JSONL lines. A `case-ready` record links the case, `case-error` reports a failed write, and `run-settled` records final counts in `aggregates.executions`, `aggregates.cases`, `aggregates.summaries` and `aggregates.replayed_ends`.

If the count becomes known later, repeat `test-log begin` with the same domain and run ID and supply `--planned-executions N`. The command still requires `--jsonl-path`. The broker continues writing to the original journal and ignores that path and other settings on the repeat. The open run records the count in a `test-log-plan` journal event and publishes it in status and list. An identical count is idempotent, and a different count for an existing plan is refused with `planned_executions_conflict`.

## Per-selector telemetry limit

`test-log begin --execution-limit-percent P` sets a per-selector share of each ring’s capacity at begin. The default is 25%. The broker rounds down and counts incoming logs and spans independently while an execution with that selector is active. Executions registered under the same selector field and value share the quota. A selector faults when its own count first exceeds either limit. Ring fullness alone cannot produce the fault.

The crossing captures the selector’s retained records immediately. The case can be bare when the snapshot slots or capture pool are exhausted. The journal and report mark every execution sharing the selector with `excessive_telemetry` and preserve the runner’s verdict. The live and settled progress counters include `faults.excessive_telemetry`.

After the crossing, a ring can admit up to the smaller of 1,000 Error records and that ring’s limit for the selector. Spans with Error status qualify. A record that also matches an active under-limit selector is kept without spending an over-limit selector’s allowance. The quota bounds admissions attributable to a selector. Records already present before registration and records shared with other selectors can also be resident. Live ring shrinking can change what fraction is resident. Existing records age out under ordinary ring eviction.

Each over-limit selector can retain the latest 200 logs and 200 spans seen from the crossing onward outside the rings. Both admitted and refused records can appear in the tail. One 16 MiB budget per run charges encoded payload bytes, index overhead, and queued tail writes. Under pressure, the oldest buffered records across selectors leave first. End rows report refusal counts separately from loss, as well as the number of missing records from each retained tail window. The end row and offline report link the tail file beside the case. A run settles after its tail writes are resolved.

## Error handling and compatibility

A successful read of an idle lane returns null. JSON CLI failures use a nonzero exit and a structured error on stdout. MCP tool errors use `isError` with a structured payload. Branch on `error.code`. A missing run gives this response.

```json
{"schema_version":1,"error":{"code":"run_not_found","message":"Run build-42 is not retained","domain":"ci","run_id":"build-42"}}
```

Use `unknown_domain` for an absent lane, `run_not_found` for an explicit ID absent from an existing lane, and `broker_unreachable` for a connection failure or timeout. Consult retained files for older runs. For other errors show status unavailable. An unsupported or absent schema version means that the progress contract is unavailable. Clients should require the supported version and required fields and tolerate extra optional fields. Changes to types, meanings, null behavior or closed value sets require a shared version bump.

## Reproduce the polling benchmark

Build the release broker and CLI with `cargo build --release --all-features`, then run `python3 scripts/bench-test-log-progress.py target/release`. The script starts an isolated broker with eight domains, sends logs, spans and lifecycle calls from eight workers in each domain, and compares two runs of the same workload. The second run launches separate CLI processes for domain discovery, current status on every lane, and retained history on one lane every two seconds. The output reports wall time and latency percentiles including process startup. Both runs verify final counts and the transition back to idle.

A parent may set its session default with `use_domain`. A subagent using a second lane passes that lane as `domain` on begin, start, end, replay, machine-load, close, and seal. Pass it on status and list as well. Each lifecycle reply identifies its serving domain. Clearing logs also accepts an explicit domain.
