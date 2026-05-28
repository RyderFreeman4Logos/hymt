#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"

if command -v uv >/dev/null 2>&1; then
  uv tool uninstall hymt 2>/dev/null || true
fi

echo "post-push: reinstalling hymt (Rust) from ${repo_root}..." >&2
if cargo install --path "${repo_root}/crates/hymt-cli" --force 2>&1; then
  exit 0
fi
echo "post-push: cargo install failed, continuing." >&2
