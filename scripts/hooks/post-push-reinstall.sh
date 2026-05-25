#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"

if command -v mise >/dev/null 2>&1; then
  echo "post-commit: reinstalling hymt from ${repo_root} via mise..." >&2
  if mise x -- uv tool install --force "${repo_root}" 2>&1; then
    exit 0
  fi
  echo "post-commit: mise reinstall failed, continuing." >&2
  exit 0
fi

if command -v uv >/dev/null 2>&1; then
  echo "post-commit: reinstalling hymt from ${repo_root} via uv..." >&2
  if uv tool install --force "${repo_root}" 2>&1; then
    exit 0
  fi
  echo "post-commit: reinstall failed, continuing." >&2
  exit 0
fi

echo "post-commit: mise/uv not found, skipping hymt reinstall." >&2
