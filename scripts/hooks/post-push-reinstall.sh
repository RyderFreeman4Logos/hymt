#!/usr/bin/env bash
set -euo pipefail

repo_url="$(git remote get-url origin)"
branch="$(git rev-parse --abbrev-ref HEAD)"
install_target="git+${repo_url}@${branch}"

if command -v mise >/dev/null 2>&1; then
  echo "post-push: reinstalling hymt from ${repo_url}@${branch} via mise..." >&2
  if mise x -- uv tool install --force "${install_target}" 2>&1; then
    exit 0
  fi
  echo "post-push: mise reinstall failed, continuing." >&2
  exit 0
fi

if command -v uv >/dev/null 2>&1; then
  echo "post-push: reinstalling hymt from ${repo_url}@${branch} via uv..." >&2
  if uv tool install --force "${install_target}" 2>&1; then
    exit 0
  fi
  echo "post-push: reinstall failed, continuing." >&2
  exit 0
fi

echo "post-push: mise/uv not found, skipping hymt reinstall." >&2
