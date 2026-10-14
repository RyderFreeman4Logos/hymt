#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "${repo_root}"

echo "post-push: reinstalling hymt (Rust) from ${repo_root}..." >&2
exec just install
