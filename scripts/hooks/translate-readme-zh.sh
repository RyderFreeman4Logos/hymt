#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
readme_en="${README_SOURCE:-${repo_root}/README.md}"
readme_zh="${repo_root}/README.zh-cn.md"
real_home="${HOME:-}"
runtime_home="${repo_root}/.git/hymt-home"

prepare_runtime_home() {
  mkdir -p \
    "${runtime_home}/.config/hymt" \
    "${runtime_home}/.cache/hymt" \
    "${runtime_home}/.local/share/hymt"
  if [ -n "${real_home}" ] && [ -f "${real_home}/.config/hymt/config.toml" ]; then
    cp "${real_home}/.config/hymt/config.toml" \
      "${runtime_home}/.config/hymt/config.toml"
  fi
}

run_hymt() {
  prepare_runtime_home
  if command -v hymt >/dev/null 2>&1 && hymt --help 2>/dev/null | grep -q "translate-doc"; then
    HOME="${runtime_home}" hymt "$@"
    return 0
  fi
  if [ -x "${repo_root}/.venv/bin/python" ]; then
    HOME="${runtime_home}" \
      PYTHONPATH="${repo_root}/src" \
      "${repo_root}/.venv/bin/python" -m hymt "$@"
    return 0
  fi
  return 127
}

set +e
run_hymt translate-doc "${readme_en}" -l zh --output "${readme_zh}"
status=$?
set -e

if [ "${status}" -eq 0 ]; then
  exit 0
fi

if [ "${status}" -eq 127 ]; then
  echo "SKIP doc-translate-sync: hymt is not installed in PATH and no repo .venv is available." >&2
  exit 0
fi
echo "SKIP doc-translate-sync: translate-doc failed (endpoint unavailable or translation error)." >&2
exit 0
