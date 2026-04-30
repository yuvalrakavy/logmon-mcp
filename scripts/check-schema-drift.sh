#!/usr/bin/env bash
# Pre-commit / CI hook: fail if the committed protocol-v1.schema.json
# diverges from what xtask gen-schema would produce.
set -euo pipefail
cargo xtask verify-schema
