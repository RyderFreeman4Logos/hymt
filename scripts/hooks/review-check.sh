#!/usr/bin/env bash
# Validate exact-HEAD local-gate and whole-branch native-review receipts.
set -euo pipefail
ROOT="$(git rev-parse --show-toplevel)"
exec python3 "$ROOT/scripts/hooks/local-gates.py" verify
