#!/usr/bin/env bash
# Full pre-commit verification. Three rules, each bought with session time:
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
# 3. EVERY STEP REPORTS ITS OWN WALL CLOCK. A session spent ~90 minutes across
#    five runs of this script and could not say which step it was waiting on,
#    so the first attempt to speed it up measured `cargo test --workspace`
#    WITHOUT `--all-features` — a different command from the one below, with a
#    different fingerprint and a different answer. A step that does not report
#    its own cost gets optimised by guesswork.
#
# Usage: scripts/verify.sh
# Exit code is non-zero if any step fails; per-step logs are kept for grepping.

set -u
cd "$(dirname "$0")/.."

d="$(mktemp -d "${TMPDIR:-/tmp}/logmon-verify.XXXXXX")"
fail=0
started=$SECONDS

hms() { printf '%2dm%02ds' $(($1 / 60)) $(($1 % 60)); }

run() {
  local name="$1"
  shift
  printf '%-10s' "$name"
  local t0=$SECONDS status=ok
  if ! "$@" >"$d/$name.log" 2>&1; then
    status=FAILED
    fail=1
  fi
  local elapsed=$((SECONDS - t0))
  printf '%-8s %s\n' "$status" "$(hms $elapsed)"
  [ "$status" = ok ] || echo "          log: $d/$name.log"
}

# Gatekeeper assesses every NEWLY LINKED executable on its FIRST execution:
# ~20-40s apiece, at zero in-process CPU, cached per file forever after. The
# workspace has ~71 test binaries and `cargo test` runs them ONE AT A TIME, so
# those assessments serialise into the bulk of this script's wall clock — and
# they are invisible in per-suite timings, which only start counting once a
# test does. That is how "the tests take 18 seconds" and "the step takes 25
# minutes" were both true, and why the first attempt to explain this blamed
# compilation.
#
# They do parallelise. Measured on this machine, six freshly linked binaries:
# 125s one after another, 27s all at once. So the first executions are bought
# concurrently here, and `tests` below then finds every binary already cleared.
#
# Nothing is skipped and no security setting is changed — the same assessments
# happen, just not in single file. `--list` loads and exits without running a
# test, which is all it takes to spend the verdict.
#
# `|| echo x` per child is deliberate. BSD xargs ABORTS the whole batch when a
# child exits 255, so one unhappy binary silently leaves most of the set
# unwarmed and the step looks like it ran. Every child now exits 0, the
# failures are counted, and the step fails loudly if there were any.
prewarm() {
  local list="$d/prewarm.list" count fails
  cargo test --workspace --all-features --no-run --message-format=json 2>/dev/null |
    python3 scripts/prewarm-targets.py | sort -u >"$list"
  count=$(wc -l <"$list" | tr -d ' ')
  echo "clearing $count test harnesses"
  fails=$(xargs -P 8 -I{} sh -c '"$1" --list >/dev/null 2>&1 || echo x' _ {} <"$list" |
    wc -l | tr -d ' ')
  [ "$fails" -eq 0 ] || {
    echo "$fails of $count harnesses failed to run"
    return 1
  }
}

run fmt cargo fmt --all --check
run schema cargo run -q -p xtask -- verify-schema
run clippy cargo clippy --workspace --all-targets --all-features
run build cargo test --workspace --all-features --no-run
run prewarm prewarm
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

# Compilation against execution. The tests themselves have run in ~20s while
# the step took minutes; without both numbers the gap is invisible and every
# proposal to speed this up is a guess about which half to attack.
if [ -f "$d/tests.log" ]; then
  ran=$(grep -oE 'finished in [0-9.]+s' "$d/tests.log" |
    awk '{gsub(/s$/, "", $3); s += $3} END {printf "%.1f", s + 0}')
  echo "wall:  $(hms $((SECONDS - started))) total, of which ${ran}s was tests executing"
fi
echo "logs: $d"

exit $fail
