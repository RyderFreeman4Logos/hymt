#!/usr/bin/env bash
set -euo pipefail

# Shared with the post-commit wrapper so private files outlive the final consumer.
readme_identity() {
  local identity
  identity="$(stat -c '%d:%i:%u:%f' -- "$1")" || return $?
  printf '%s:%s\n' "${identity%:*}" "$((16#${identity##*:} & 0170000))"
}

remember_readme_path() {
  owned_readme_paths+=("$1")
  owned_readme_identities+=("$(readme_identity "$1")")
}

readme_source_is_current() {
  local commit ref
  commit="$(git rev-parse HEAD)" || return $?
  ref="$(git rev-parse --symbolic-full-name HEAD)" || return $?
  [ "${commit}" = "${readme_trigger_commit:-}" ] && [ "${ref}" = "${readme_trigger_ref:-}" ]
}

cleanup_readme_workspace() {
  local status=$? index preserve=0
  trap - EXIT
  set +e
  if [ -n "${publication_temp}" ] && [ "$(readme_identity "${publication_temp}" 2>/dev/null)" = "${publication_identity}" ]; then
    rm -f -- "${publication_temp}"
  fi
  for index in "${!owned_readme_paths[@]}"; do
    if { [ -e "${owned_readme_paths[index]}" ] || [ -L "${owned_readme_paths[index]}" ]; } &&
      [ "$(readme_identity "${owned_readme_paths[index]}" 2>/dev/null)" != "${owned_readme_identities[index]}" ]; then
      preserve=1
    fi
  done
  if [ "${preserve}" -eq 0 ] && [ "$(readme_identity "${readme_workspace}" 2>/dev/null)" = "${workspace_identity}" ]; then
    rm -rf -- "${readme_workspace}"
  else
    echo "WARNING doc-translate-sync: temporary path identity changed; preserving replacement." >&2
  fi
  exit "${status}"
}

initialize_readme_lifecycle() {
  real_home="${HOME:-}"
  readme_zh="${repo_root}/README.zh-cn.md"
  output_umask="$(umask)"
  umask 077
  readme_workspace=""
  workspace_identity=""
  publication_temp=""
  publication_identity=""
  published_readme_hash=""
  owned_readme_paths=()
  owned_readme_identities=()
}

prepare_readme_workspace() {
  local root ancestor mode
  [ -n "${TMPDIR:-}" ] || [ -n "${real_home}" ] || return 1
  case "${TMPDIR:-${real_home}/tmp}" in
    /tmp|/tmp/*|/var/tmp|/var/tmp/*|*/.git|*/.git/*) return 1 ;;
  esac
  root="$(readlink -f -- "${TMPDIR:-${real_home}/tmp}")" || return $?
  case "${root}" in */.git|*/.git/*) return 1 ;; esac
  [ -d "${root}" ] && [ -w "${root}" ] || return 1
  [ "$(stat -c %u -- "${root}")" = "${UID}" ] && [ "$(stat -c %a -- "${root}")" = 700 ] || return 1
  [ "$(stat -c %d -- "${root}")" = "$(stat -c %d -- /mnt/ssd)" ] || return 1
  ancestor="${root}"
  while :; do
    mode="$(stat -c %a -- "${ancestor}")" || return $?
    if [ "$((8#${mode} & 0022))" -ne 0 ] && [ "$((8#${mode} & 01000))" -eq 0 ]; then
      return 1
    fi
    [ "${ancestor}" = / ] && break
    ancestor="${ancestor%/*}"
    [ -n "${ancestor}" ] || ancestor=/
  done
  readme_workspace="$(mktemp -d "${root}/hymt-readme-XXXXXXXX")" || return $?
  trap cleanup_readme_workspace EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM
  workspace_identity="$(readme_identity "${readme_workspace}")" || return $?
  runtime_home="${readme_workspace}/hymt-home"
}

prepare_runtime_home() {
  local path
  mkdir -p "${runtime_home}/.config/hymt" "${runtime_home}/.cache/hymt" "${runtime_home}/.local/share/hymt" || return $?
  for path in "${runtime_home}" "${runtime_home}/.config" "${runtime_home}/.config/hymt" \
    "${runtime_home}/.cache" "${runtime_home}/.cache/hymt" "${runtime_home}/.local" \
    "${runtime_home}/.local/share" "${runtime_home}/.local/share/hymt"; do
    remember_readme_path "${path}" || return $?
  done
  if [ -n "${real_home}" ] && [ -f "${real_home}/.config/hymt/config.toml" ]; then
    (set -o noclobber; : > "${runtime_home}/.config/hymt/config.toml") || return $?
    remember_readme_path "${runtime_home}/.config/hymt/config.toml" || return $?
    cp "${real_home}/.config/hymt/config.toml" "${runtime_home}/.config/hymt/config.toml" || return $?
  fi
}

run_hymt() {
  local status
  prepare_runtime_home || {
    status=$?
    echo "WARNING doc-translate-sync: runtime preparation failed (exit ${status})." >&2
    return "${status}"
  }
  if command -v hymt >/dev/null 2>&1 && hymt --help 2>/dev/null | grep -q "translate-doc"; then
    HOME="${runtime_home}" TMPDIR="${readme_workspace}" TMP="${readme_workspace}" TEMP="${readme_workspace}" hymt "$@"
    status=$?
  elif [ -x "${repo_root}/.venv/bin/python" ]; then
    HOME="${runtime_home}" TMPDIR="${readme_workspace}" TMP="${readme_workspace}" TEMP="${readme_workspace}" \
      PYTHONPATH="${repo_root}/src" "${repo_root}/.venv/bin/python" -m hymt "$@"
    status=$?
  else
    echo "WARNING doc-translate-sync: compatible translator unavailable in PATH or repo .venv." >&2
    return 127
  fi
  if [ "${status}" -ne 0 ]; then
    echo "WARNING doc-translate-sync: translation failed (exit ${status})." >&2
  fi
  return "${status}"
}

capture_readme_destination() {
  output_identity=""
  output_hash=""
  output_mode="$((0666 & ~8#${output_umask}))"
  if [ -e "${readme_zh}" ] || [ -L "${readme_zh}" ]; then
    [ ! -L "${readme_zh}" ] && [ -f "${readme_zh}" ] || return 1
    output_identity="$(readme_identity "${readme_zh}")" || return $?
    output_mode="$(stat -c %a -- "${readme_zh}")" || return $?
    [ "$((8#${output_mode}))" -le "$((0777))" ] || return 1
    output_mode="$((8#${output_mode}))"
    output_hash="$(sha256sum < "${readme_zh}")" || return $?
  fi
}

publish_readme_candidate() {
  local current current_hash
  [ -f "${readme_candidate}" ] && [ ! -L "${readme_candidate}" ] && [ "$(stat -c %u -- "${readme_candidate}")" = "${UID}" ] || return 1
  publication_temp="$(mktemp "${repo_root}/.README.zh-cn.md.hymt-XXXXXXXX")" || return $?
  publication_identity="$(readme_identity "${publication_temp}")" || return $?
  cp "${readme_candidate}" "${publication_temp}" || return $?
  [ "$(readme_identity "${publication_temp}")" = "${publication_identity}" ] || return 1
  published_readme_hash="$(sha256sum < "${publication_temp}")" || return $?
  chmod "$(printf '%o' "${output_mode}")" -- "${publication_temp}" || return $?
  current=""
  current_hash=""
  if [ -e "${readme_zh}" ] || [ -L "${readme_zh}" ]; then
    current="$(readme_identity "${readme_zh}")" || return $?
    current_hash="$(sha256sum < "${readme_zh}")" || return $?
    [ "$(stat -c %a -- "${readme_zh}")" = "$(printf '%o' "${output_mode}")" ] || return 1
  fi
  [ "${current}" = "${output_identity}" ] && [ "${current_hash}" = "${output_hash}" ] || return 1
  [ -z "$(git diff --cached -- README.zh-cn.md)" ] || return 1
  [ "$(readme_identity "${publication_temp}")" = "${publication_identity}" ] || return 1
  if [ -n "${readme_trigger_commit:-}" ]; then
    readme_source_is_current || return 1
  fi
  # The candidate is on SSD; this complete, closed copy shares the destination device.
  mv -T -- "${publication_temp}" "${readme_zh}" || return $?
  publication_temp=""
}

translate_readme() {
  local status index
  capture_readme_destination || {
    status=$?
    echo "WARNING doc-translate-sync: output preparation failed (exit ${status})." >&2
    return "${status}"
  }
  readme_candidate="$(mktemp "${readme_workspace}/candidate-XXXXXXXX.md")" || {
    status=$?
    echo "WARNING doc-translate-sync: output preparation failed (exit ${status})." >&2
    return "${status}"
  }
  remember_readme_path "${readme_candidate}" || return $?
  index=$((${#owned_readme_paths[@]} - 1))
  status=0
  run_hymt translate-doc "$1" -l zh --output "${readme_candidate}" || status=$?
  [ "${status}" -eq 0 ] || return "${status}"
  # A compatible writer may atomically replace its own regular candidate.
  if [ -f "${readme_candidate}" ] && [ ! -L "${readme_candidate}" ] && [ "$(stat -c %u -- "${readme_candidate}")" = "${UID}" ]; then
    owned_readme_identities[index]="$(readme_identity "${readme_candidate}")" || return $?
  fi
  publish_readme_candidate || {
    status=$?
    echo "WARNING doc-translate-sync: output publication failed (exit ${status})." >&2
    return "${status}"
  }
  if [ "$(sha256sum < "${readme_zh}")" != "${published_readme_hash}" ]; then
    echo "WARNING doc-translate-sync: output changed after publication." >&2
    return 1
  fi
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  repo_root="$(git rev-parse --show-toplevel)"
  initialize_readme_lifecycle
  prepare_readme_workspace || {
    status=$?
    echo "WARNING doc-translate-sync: runtime preparation failed (exit ${status})." >&2
    exit "${status}"
  }
  status=0
  translate_readme "${README_SOURCE:-${repo_root}/README.md}" || status=$?
  exit "${status}"
fi
