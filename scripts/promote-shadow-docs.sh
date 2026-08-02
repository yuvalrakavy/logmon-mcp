#!/usr/bin/env bash
# Promote reviewed shadow docs over their live files.
#
# Docs for an unreleased change are written to `docs/shadow/` first so they can
# be read as a diff against what ships today, rather than landing in the live
# files and making `git diff` useless for review.
#
# RUN THIS BEFORE `cargo install`, NOT AFTER.
#
#   crates/mcp/src/server.rs does `include_str!("../../../skill/logmon.md")`.
#   The skill is COMPILED INTO the shim and served as MCP `instructions`, so a
#   shadow promoted after the build ships nothing: the binary already has the
#   old text. The failure is silent — no error, no warning, just an agent that
#   never learns the feature exists.
#
# Idempotent: promoting twice is a no-op, and an absent shadow is not an error.

set -euo pipefail
cd "$(dirname "$0")/.."

# shadow path -> live path
PAIRS=(
  "docs/shadow/README.md:README.md"
  "docs/shadow/skill-logmon.md:skill/logmon.md"
  "docs/shadow/CHANGELOG.md:CHANGELOG.md"
)

promoted=0
for pair in "${PAIRS[@]}"; do
  shadow="${pair%%:*}"
  live="${pair##*:}"
  if [[ ! -f "$shadow" ]]; then
    continue
  fi
  if cmp -s "$shadow" "$live"; then
    echo "unchanged  $live"
    rm "$shadow"
    continue
  fi
  cp "$shadow" "$live"
  rm "$shadow"
  echo "promoted   $live"
  promoted=$((promoted + 1))
done

if [[ $promoted -eq 0 ]]; then
  echo
  echo "Nothing to promote. If you expected changes, check that docs/shadow/"
  echo "still holds them — they may already have been promoted."
  exit 0
fi

echo
echo "$promoted file(s) promoted. Now, IN THIS ORDER:"
echo "  1. git diff        — review what changed in the live files"
echo "  2. cargo test --workspace"
echo "  3. commit"
echo "  4. cargo install --path crates/broker   (broker FIRST)"
echo "  5. confirm it is serving"
echo "  6. cargo install --path crates/mcp"
echo
echo "Step 4 is what bakes skill/logmon.md into the shim. Promoting after it"
echo "installs a binary that still carries the old instructions."
