# Replaying a Store nightly workload

The transport fixture checks capture and reload. Store’s rollout gate measures its
actual 16-way DSL workload against the nightly’s 2% elapsed-time budget. A fixture
pass alone cannot establish the cost of enabling full summaries.

To reproduce a Store nightly selection, supply its run directory and a candidate Store
checkout with built debug binaries. First inspect the recorded batch order:

```sh
python3 scripts/bench-store-summaries.py target.noindex/debug "$STORE_CHECKOUT" \
  --nightly-run "$NIGHTLY_RUN_DIR" --plan-only
```

For a supervised measurement replace `--plan-only` with `--pairs 3 --records "$NEW_DIRECTORY"`.
If full summaries exceed the 2% budget, add `--summary-percent P` to compare the
disabled arm with stable-name sampling at P percent.
For a rehearsal use `--batch NUMBER --single-arm 100 --records "$NEW_DIRECTORY"`.
The paired run freezes the binaries and inputs and uses private brokers, Store servers
and template copies. Compare completed tests, sender discards, receiver drops,
acknowledgement p99, and DSL wall time for each arm. A representative production
run may take several hours per arm.
