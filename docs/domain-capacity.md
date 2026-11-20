# Sizing a telemetry domain

Create a disposable domain for a test run. The broker returns its receiver ports. Send the application's logs and spans to those ports.

```sh
logmon-mcp domains create --name ci --log-buffer-size 5000 --span-buffer-size 20000 --max-log-buffer-size 10000 --max-span-buffer-size 80000
logmon-mcp domains list
logmon-mcp --json domains list
```

Each ring starts with its own configured record capacity. Omitting its maximum keeps it fixed. A full ring normally evicts the oldest record by broker sequence. If an active execution or an execution in its post-verdict grace period needs the record, the affected ring can grow in steps up to its maximum, preserving parallel and untagged activity as well as the execution itself. A case that already secured its snapshot releases its need for this protection. At the ceiling the ring evicts normally. Inspect the log and span coverage separately in the case. The current allocation can outlast the execution.

The list reports initial, current and maximum capacities alongside occupancy and estimated domain memory in bytes. In JSON, the memory breakdown attributes record data to each ring, ring/index overhead to indexes, and additional allocations for queued captures to pending captures. Records shared between cases and the ring contribute once. Allocation, payload sizes and index cardinality affect the estimate. A representative average for your own records is the change in estimated record bytes divided by records added during a steady sample. For example, one archived integration-test case held 737 logs at 3,048 estimated bytes per log and 10,135 spans at 3,021 bytes per span, plus 351,360 bytes of indexes. Its total was 33,218,889 bytes. Use this as a starting point only for records with similar payloads. Process RSS also includes the runtime, allocator and receiver queues.

## Resize and retry

```sh
logmon-mcp domains resize --name ci --span-buffer-size 40000 --max-span-buffer-size 100000
logmon-mcp --domain ci domains clear
logmon-mcp domains delete --name ci --if-exists
```

Create accepts omitted sizes on an existing domain and compares supplied values against the configuration. If sizes differ, it returns a capacity mismatch (CLI exit 3) with requested and current settings. Resize preserves receiver ports, sequence numbers, and the newest records per ring. The result includes before/after statistics and evictions. Inspect the clear response for memory before and after, or delete for the final estimate. With `--if-exists`, cleanup also succeeds when the broker confirms absence. A connectivity or persistence error returns a failure.

## Surviving broker restarts

Use `domains create --persist` when producers will outlive the broker, for example a development service sending to a stable port. The broker saves the name, actual ports and configured capacities and restarts the listeners with empty rings. An existing ephemeral domain needs to be deliberately replaced to switch its lifetime. Explicit deletion removes the saved definition. Inspect broker diagnostics if another process occupies a saved port. An unreachable UDP receiver can lose records silently, while TCP/OTLP delivery depends on the producer's retry behavior. A disposable test run can use the ephemeral default and recreate its domain when starting a new run.

On a shared MCP connection, `clear_domain(domain="staging")` clears both rings in staging. The reply identifies the domain and gives the cleared log and span counts. Omitting `domain` uses the session default. Loaded cases refuse this operation.
