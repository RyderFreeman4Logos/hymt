#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "${repo_root}"

readme_trigger_commit="$(git rev-parse HEAD)" || {
  echo "post-commit: could not identify triggering commit; skipping auto-translation." >&2
  exit 0
}
readme_trigger_ref="$(git rev-parse --symbolic-full-name HEAD)" || {
  echo "post-commit: could not identify triggering branch; skipping auto-translation." >&2
  exit 0
}
changed_files="$(git diff-tree --no-commit-id --name-only -r "${readme_trigger_commit}")" || {
  echo "post-commit: could not inspect triggering commit; skipping auto-translation." >&2
  exit 0
}
if ! printf '%s\n' "${changed_files}" | grep -qx "README.md"; then
  exit 0
fi
if printf '%s\n' "${changed_files}" | grep -qx "README.zh-cn.md"; then
  echo "post-commit: README.zh-cn.md changed manually; skipping auto-translation." >&2
  exit 0
fi

if [ -n "$(git diff -- README.zh-cn.md)" ] || [ -n "$(git diff --cached -- README.zh-cn.md)" ]; then
  echo "post-commit: README.zh-cn.md has uncommitted changes; skipping auto-translation." >&2
  exit 0
fi

source "${repo_root}/scripts/hooks/translate-readme-zh.sh"
initialize_readme_lifecycle
prepare_readme_workspace || {
  status=$?
  echo "post-commit: runtime preparation failed (exit ${status}); skipping auto-translation." >&2
  exit 0
}
readme_src=$(mktemp "${readme_workspace}/README-committed-XXXXXXXX.md") || {
  status=$?
  echo "post-commit: snapshot preparation failed (exit ${status}); skipping auto-translation." >&2
  exit 0
}
remember_readme_path "${readme_src}"
git show "${readme_trigger_commit}:README.md" > "${readme_src}" || {
  status=$?
  echo "post-commit: snapshot preparation failed (exit ${status}); skipping auto-translation." >&2
  exit 0
}
translation_status=0
translate_readme "${readme_src}" || translation_status=$?
if [ "${translation_status}" -ne 0 ]; then
  echo "post-commit: README translation skipped after helper failure (exit ${translation_status})." >&2
  exit 0
fi
if [ -z "$(git status --porcelain -- README.zh-cn.md)" ]; then
  echo "post-commit: README.zh-cn.md already in sync." >&2
  exit 0
fi

mkdir "${readme_workspace}/just-tmp" || {
  status=$?
  echo "post-commit: commit preparation failed (exit ${status}); skipping auto-translation commit." >&2
  exit 0
}
remember_readme_path "${readme_workspace}/just-tmp"
if ! readme_source_is_current; then
  echo "post-commit: HEAD changed during translation; skipping auto-translation commit." >&2
  exit 0
fi
if [ -n "$(git diff --cached -- README.zh-cn.md)" ] ||
  [ "$(sha256sum < "${repo_root}/README.zh-cn.md")" != "${published_readme_hash}" ]; then
  echo "post-commit: README.zh-cn.md changed during translation; skipping auto-translation commit." >&2
  exit 0
fi
env JUST_TEMPDIR="${readme_workspace}/just-tmp" \
  git commit --only -m "docs(readme): auto-translate README.zh-cn.md" -- README.zh-cn.md
