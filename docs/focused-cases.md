# Investigating a run archive

Extract the run ZIP and open `summary.md`. Follow an entry from `failed-test-log.jsonl` to its case document. The relative path starts in the directory containing `full-test-log.jsonl`. Keep the case Markdown beside its log, span and registry sidecars.

Use the absolute Markdown path in these MCP calls, substituting the path from the journal.

```text
load_case(path="/tmp/run/cases/example--ID/example--ID.md", scope="focus", domain="execution")
use_domain(name="execution")
get_recent_logs(count=20)
load_case(path="/tmp/run/cases/example--ID/example--ID.md", domain="whole-window")
use_domain(name="whole-window")
```

The focus domain contains records tagged with the declared trace ID. The full window includes direct peers’ earlier activity, untagged records and machine samples. For a shared deadline case, choose an execution in the crossings table and pass its trace ID as `focus_trace_id` with `scope="focus"`. A captured deadline crossing can be followed by an OK verdict. Read the final execution record for the outcome.

For manual capture, pass `focus_trace_id` alongside the usual `anchor`, `dir` and `reason` to `create_case`. Manual cases infer overlap from retained trace intervals. Test cases use execution start and end markers and can name peers whose telemetry has rolled out. The Markdown shows a bounded preview per trace. Query the loaded domain or open the raw sidecars for the full saved records.

Use `logmon-test-log compare` with the original execution and its linked rerun. Read `metric_coverage` for logs and spans independently. A backfilled failure can have incomplete spans and complete logs. The comparison suppresses percentage changes for metrics with incomplete coverage while retaining measured values. A rerun that passes still supplies a baseline.
