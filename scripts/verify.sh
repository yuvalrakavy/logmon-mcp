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

# WHERE THE WALL CLOCK GOES, and what did NOT fix it.
#
# Gatekeeper assesses every NEWLY LINKED executable on its FIRST execution:
# ~20-40s apiece, at zero in-process CPU, cached per file thereafter. This
# workspace has 68 test harnesses, so any change to an upstream crate —
# `crates/protocol` above all — relinks the lot and owes the assessment again.
# It is invisible in per-suite timings, which start counting only once a test
# does, which is how "the tests take 18 seconds" and "the step takes 25
# minutes" were both true and the first explanation blamed compilation.
#
# A pre-warm step that ran all 68 concurrently before `tests` was TRIED AND
# REMOVED. It did not work, in two independent ways:
#
#   * No speedup at scale. Six binaries measured 125s serial against 27s
#     concurrent, so 68 was expected to collapse. It took 44m00s — about 39s
#     each, indistinguishable from serial. syspolicyd evidently does not
#     parallelise beyond a handful, and extrapolating n=6 to n=68 was exactly
#     the mistake of quoting a number measured in another context.
#   * `tests` did not get cheaper anyway. It still took 8m45s with every
#     binary already cleared, against 20.4s of actual execution — so clearing
#     them is not what that step is waiting on either.
#
# Net: 62m57s with the pre-warm against 27m58s without. Recorded here rather
# than deleted, so the next person reading these numbers does not re-derive it.
#
# The remedy that does work is a machine setting, not a script change:
# System Settings -> Privacy & Security -> Developer Tools, listing whatever
# application runs cargo. That exempts spawned processes from assessment
# outright. It needs that app restarted before it takes effect.
run fmt cargo fmt --all --check
run schema cargo run -q -p xtask -- verify-schema
run clippy cargo clippy --workspace --all-targets --all-features
run build cargo test --workspace --all-features --no-run
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
