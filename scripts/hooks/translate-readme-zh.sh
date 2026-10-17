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
  commit="$(git -C "${repo_root}" rev-parse HEAD)" || return $?
  ref="$(git -C "${repo_root}" rev-parse --symbolic-full-name HEAD)" || return $?
  [ "${commit}" = "${readme_trigger_commit:-}" ] && [ "${ref}" = "${readme_trigger_ref:-}" ]
}

# The setsid child stays the exact process-group leader until all group
# signals have been sent. Its own worker has default signal dispositions.
readme_consumer_leader_state() {
  local record
  local -a fields
  [ -n "${readme_consumer_start}" ] || return 1
  IFS= read -r record 2>/dev/null < "/proc/${readme_consumer_pid}/stat" || return 1
  read -r -a fields <<< "${record##*) }"
  [ "${fields[19]:-}" = "${readme_consumer_start}" ] || return 1
  readme_consumer_leader_status="${fields[0]:-}"
  readme_consumer_group_owned=0
  [ "${fields[2]:-}" = "${readme_consumer_pid}" ] &&
    [ "${fields[3]:-}" = "${readme_consumer_pid}" ] || return 1
  readme_consumer_group_owned=1
}

observe_readme_consumer() {
  local record attempt
  local -a fields
  for ((attempt=0; attempt<20; attempt++)); do
    if [ -z "${readme_consumer_start}" ]; then
      IFS= read -r record 2>/dev/null < "/proc/${readme_consumer_pid}/stat" || return 0
      read -r -a fields <<< "${record##*) }"
      readme_consumer_start="${fields[19]:-}"
    fi
    readme_consumer_leader_state && return 0
    [ "${readme_consumer_leader_status:-}" = Z ] && return 0
    /usr/bin/sleep 0.01
  done
}

# Return success only for a live group member. Zombie-only groups cannot
# access the workspace. The leader is excluded while deciding its release.
readme_consumer_group_has_live() {
  local exclude_leader="$1" entry member record
  local -a fields
  for entry in /proc/[0-9]*/stat; do
    member="${entry#/proc/}"
    member="${member%/stat}"
    [ "${exclude_leader}" -eq 1 ] && [ "${member}" = "${readme_consumer_pid}" ] && continue
    if ! IFS= read -r record 2>/dev/null < "${entry}"; then
      [ -e "${entry}" ] && return 2
      continue
    fi
    read -r -a fields <<< "${record##*) }"
    [ "${fields[2]:-}" = "${readme_consumer_pid}" ] || continue
    [ "${fields[0]:-}" = Z ] || return 0
  done
  return 1
}

# This check sends no signal. Reap only after the entire original group has
# no live members; all TERM/KILL operations are already finished by then.
readme_consumer_is_settled() {
  local state
  if readme_consumer_group_has_live 0; then
    return 1
  else
    state=$?
    [ "${state}" -eq 1 ] || return 1
  fi
  if [ "${readme_consumer_reaped}" -eq 0 ]; then
    wait "${readme_consumer_pid}" 2>/dev/null || :
    readme_consumer_reaped=1
  fi
  return 0
}

stop_readme_consumer() {
  local pid="${readme_consumer_pid:-}" attempt released=0 state
  [ -n "${pid}" ] || return 0
  observe_readme_consumer
  # No PGID operation is allowed without the original leader start time.
  readme_consumer_leader_state && [ "${readme_consumer_group_owned}" -eq 1 ] || return 1
  kill -TERM -- "-${pid}" 2>/dev/null || :
  for ((attempt=0; attempt<20; attempt++)); do
    readme_consumer_leader_state || break
    if [ -s "${readme_consumer_status_file}" ]; then
      if readme_consumer_group_has_live 1; then
        :
      else
        state=$?
        if [ "${state}" -eq 1 ]; then
          : > "${readme_consumer_status_file}.release" || return 1
          released=1
          break
        fi
      fi
    fi
    /usr/bin/sleep 0.1
  done
  if [ "${released}" -eq 1 ]; then
    for ((attempt=0; attempt<20; attempt++)); do
      readme_consumer_leader_state || break
      [ "${readme_consumer_leader_status}" = Z ] && break
      /usr/bin/sleep 0.01
    done
  fi
  # A still-live leader anchors the last group KILL. Never signal this PGID
  # after its identity is missing, including on cancellation or uncertainty.
  if readme_consumer_leader_state && [ "${readme_consumer_group_owned}" -eq 1 ] &&
    [ "${readme_consumer_leader_status}" != Z ]; then
    kill -KILL -- "-${pid}" 2>/dev/null || :
  fi
  for ((attempt=0; attempt<20; attempt++)); do
    if readme_consumer_is_settled; then
      readme_consumer_settled=1
      readme_consumer_pid=""
      return 0
    fi
    /usr/bin/sleep 0.1
  done
  return 1
}

cancel_readme() {
  trap '' HUP INT TERM
  stop_readme_consumer || :
  exit "$1"
}

run_readme_consumer() {
  local status=1
  # Record signals during fork before the owned process group PID is available.
  trap 'readme_cancel_status=129' HUP
  trap 'readme_cancel_status=130' INT
  trap 'readme_cancel_status=143' TERM
  readme_consumer_status_file="$(mktemp "${readme_workspace}/consumer-status-XXXXXXXX")" || return 1
  /usr/bin/setsid -- /usr/bin/bash -c '
    trap : HUP INT TERM
    status_file=$1
    shift
    (trap - HUP INT TERM; exec "$@") &
    worker=$!
    # Keep only the worker on the caller output pipes after it starts.
    exec >/dev/null 2>&1
    while :; do
      wait "$worker"
      status=$?
      kill -0 "$worker" 2>/dev/null || break
    done
    printf "%s\n" "$status" > "$status_file"
    while [ ! -e "${status_file}.release" ]; do /usr/bin/sleep 0.05; done
  ' readme-consumer "${readme_consumer_status_file}" "$@" &
  readme_consumer_pid=$!
  readme_consumer_start=""
  readme_consumer_group_owned=0
  readme_consumer_reaped=0
  readme_consumer_settled=0
  trap 'cancel_readme 129' HUP
  trap 'cancel_readme 130' INT
  trap 'cancel_readme 143' TERM
  [ "${readme_cancel_status}" -eq 0 ] || cancel_readme "${readme_cancel_status}"
  observe_readme_consumer
  while [ ! -s "${readme_consumer_status_file}" ]; do
    readme_consumer_leader_state || break
    [ "${readme_consumer_leader_status}" = Z ] && break
    /usr/bin/sleep 0.05
  done
  if [ -s "${readme_consumer_status_file}" ]; then
    IFS= read -r status < "${readme_consumer_status_file}" || status=1
    case "${status}" in ''|*[!0-9]*) status=1 ;; esac
    [ "${status}" -le 255 ] || status=1
  fi
  stop_readme_consumer || { [ "${status}" -ne 0 ] || status=1; }
  return "${status}"
}

cleanup_readme_workspace() {
  local status=$? index preserve=0
  trap - EXIT
  trap 'status=129' HUP
  trap 'status=130' INT
  trap 'status=143' TERM
  set +e
  if [ -n "${publication_temp}" ] && [ "$(readme_identity "${publication_temp}" 2>/dev/null)" = "${publication_identity}" ]; then
    rm -f -- "${publication_temp}"
  fi
  if [ "${readme_consumer_settled}" -ne 1 ]; then
    preserve=1
    echo "WARNING doc-translate-sync: consumer settlement unconfirmed; preserving private workspace." >&2
  fi
  for index in "${!owned_readme_paths[@]}"; do
    if { [ -e "${owned_readme_paths[index]}" ] || [ -L "${owned_readme_paths[index]}" ]; } &&
      [ "$(readme_identity "${owned_readme_paths[index]}" 2>/dev/null)" != "${owned_readme_identities[index]}" ]; then
      preserve=1
    fi
  done
  if [ "${preserve}" -eq 0 ] && [ "$(readme_identity "${readme_workspace}" 2>/dev/null)" = "${workspace_identity}" ]; then
    rm -rf -- "${readme_workspace}"
  elif [ "${readme_consumer_settled}" -eq 1 ]; then
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
  published_readme_identity=""
  published_readme_mode=""
  readme_consumer_pid=""
  readme_consumer_status_file=""
  readme_consumer_start=""
  readme_consumer_group_owned=0
  readme_consumer_reaped=1
  readme_consumer_settled=1
  readme_cancel_status=0
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
  trap 'cancel_readme 129' HUP
  trap 'cancel_readme 130' INT
  trap 'cancel_readme 143' TERM
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
  local status probe="${readme_workspace}/hymt-help"
  prepare_runtime_home || {
    status=$?
    echo "WARNING doc-translate-sync: runtime preparation failed (exit ${status})." >&2
    return "${status}"
  }
  (set -o noclobber; : > "${probe}") || return $?
  remember_readme_path "${probe}" || return $?
  if command -v hymt >/dev/null 2>&1 &&
    HOME="${runtime_home}" TMPDIR="${readme_workspace}" TMP="${readme_workspace}" TEMP="${readme_workspace}" \
      run_readme_consumer hymt --help > "${probe}" 2>/dev/null && grep -q "translate-doc" "${probe}"; then
    HOME="${runtime_home}" TMPDIR="${readme_workspace}" TMP="${readme_workspace}" TEMP="${readme_workspace}" run_readme_consumer hymt "$@"
    status=$?
  elif [ "${readme_consumer_settled}" -ne 1 ]; then
    return 1
  elif [ -x "${repo_root}/.venv/bin/python" ]; then
    HOME="${runtime_home}" TMPDIR="${readme_workspace}" TMP="${readme_workspace}" TEMP="${readme_workspace}" \
      PYTHONPATH="${repo_root}/src" run_readme_consumer "${repo_root}/.venv/bin/python" -m hymt "$@"
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

published_readme_is_current() {
  [ -f "${readme_zh}" ] && [ ! -L "${readme_zh}" ] || return 1
  [ "$(readme_identity "${readme_zh}")" = "${published_readme_identity}" ] || return 1
  [ "$(stat -c %a -- "${readme_zh}")" = "${published_readme_mode}" ] || return 1
  [ "$(sha256sum < "${readme_zh}")" = "${published_readme_hash}" ]
}

publish_readme_candidate() {
  local current current_hash
  [ -f "${readme_candidate}" ] && [ ! -L "${readme_candidate}" ] && [ "$(stat -c %u -- "${readme_candidate}")" = "${UID}" ] || return 1
  publication_temp="$(mktemp "${repo_root}/.README.zh-cn.md.hymt-XXXXXXXX")" || return $?
  publication_identity="$(readme_identity "${publication_temp}")" || return $?
  cp "${readme_candidate}" "${publication_temp}" || return $?
  [ "$(readme_identity "${publication_temp}")" = "${publication_identity}" ] || return 1
  published_readme_hash="$(sha256sum < "${publication_temp}")" || return $?
  published_readme_mode="$(printf '%o' "${output_mode}")"
  chmod "${published_readme_mode}" -- "${publication_temp}" || return $?
  current=""
  current_hash=""
  if [ -e "${readme_zh}" ] || [ -L "${readme_zh}" ]; then
    [ ! -L "${readme_zh}" ] && [ -f "${readme_zh}" ] || return 1
    current="$(readme_identity "${readme_zh}")" || return $?
    [ "${current}" = "${output_identity}" ] || return 1
    [ "$(stat -c %a -- "${readme_zh}")" = "${published_readme_mode}" ] || return 1
    current_hash="$(sha256sum < "${readme_zh}")" || return $?
  fi
  [ "${current}" = "${output_identity}" ] && [ "${current_hash}" = "${output_hash}" ] || return 1
  [ -z "$(git -C "${repo_root}" diff --cached -- README.zh-cn.md)" ] || return 1
  [ "$(readme_identity "${publication_temp}")" = "${publication_identity}" ] || return 1
  if [ -n "${readme_trigger_commit:-}" ]; then
    readme_source_is_current || return 1
  fi
  # The candidate is on SSD; this complete, closed copy shares the destination device.
  mv -T -- "${publication_temp}" "${readme_zh}" || return $?
  published_readme_identity="${publication_identity}"
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
  if ! published_readme_is_current; then
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
