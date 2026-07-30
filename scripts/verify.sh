#!/usr/bin/env bash
# Full pre-commit verification. Two rules, each bought with session time:
#
# 1. EVERY EXPENSIVE STEP RUNS EXACTLY ONCE. The ad-hoc pattern
#    (`cargo test … | grep A; cargo test … | grep B`) re-runs the whole suite
#    per question asked — on 2026-07-30 one verification script ran the
#    ~10-minute workspace suite twice to produce two grep counts. Each step
#    here tees to its own log; the summary asks the logs.
#
# 2. THE SUITE RUN IS THE SUITE YOU THINK IT IS. `--all-features` is
#    load-bearing: a dozen-plus integration files (collector_end_to_end,
#    boot_resilience, domains_binding, …) are gated behind
#    `#![cfg(feature = "test-support")]`, and a plain `cargo test --workspace`
#    compiles them EMPTY and reports the suite ok. "63 green suites" once
#    included silently-empty ones. The zero-test tally below exists to catch
#    the next gate of that kind.
#
# Usage: scripts/verify.sh
# Exit code is non-zero if any step fails; per-step logs are kept for grepping.

set -u
cd "$(dirname "$0")/.."

d="$(mktemp -d "${TMPDIR:-/tmp}/logmon-verify.XXXXXX")"
fail=0

run() {
  local name="$1"
  shift
  printf '%-10s' "$name"
  if "$@" >"$d/$name.log" 2>&1; then
    echo "ok"
  else
    echo "FAILED   ($d/$name.log)"
    fail=1
  fi
}

run fmt cargo fmt --all --check
run schema cargo run -q -p xtask -- verify-schema
run clippy cargo clippy --workspace --all-targets --all-features
run tests cargo test --workspace --all-features

echo
if [ -f "$d/tests.log" ]; then
  suites=$(grep -c '^test result: ok' "$d/tests.log" || true)
  empty=$(grep -c '^test result: ok. 0 passed; 0 failed; 0 ignored' "$d/tests.log" || true)
  bad=$(grep -Ec 'FAILED|panicked at' "$d/tests.log" || true)
  total=$(grep -oE '^test result: ok\. [0-9]+' "$d/tests.log" | awk '{s+=$4} END {print s+0}')
  echo "tests: ${total} passed across ${suites} green suites (${empty} empty), ${bad} failure markers"
fi
if [ -f "$d/clippy.log" ]; then
  # Lint messages start `warning: <lowercase>`; the trailing per-crate
  # "generated N warnings" lines start with a backtick and do not match.
  lints=$(grep -Ec '^warning: [a-z]' "$d/clippy.log" || true)
  if [ "${lints:-0}" -gt 0 ]; then
    echo "clippy: ${lints} lint warnings   ($d/clippy.log)"
    fail=1
  else
    echo "clippy: clean"
  fi
fi
echo "logs: $d"

exit $fail
