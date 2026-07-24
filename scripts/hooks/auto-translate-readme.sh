#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "${repo_root}"

changed_files="$(git diff-tree --no-commit-id --name-only -r HEAD)"
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

if [ -n "$(git diff -- README.md)" ]; then
  readme_src=$(mktemp "${repo_root}/.git/README-committed-XXXXXX.md")
  git show HEAD:README.md > "${readme_src}"
  trap 'rm -f "${readme_src}"' EXIT
  README_SOURCE="${readme_src}" bash "${repo_root}/scripts/hooks/translate-readme-zh.sh"
else
  bash "${repo_root}/scripts/hooks/translate-readme-zh.sh"
fi
if [ -z "$(git status --porcelain -- README.zh-cn.md)" ]; then
  echo "post-commit: README.zh-cn.md already in sync." >&2
  exit 0
fi

mkdir -p "${repo_root}/.git/just-tmp"
env JUST_TEMPDIR="${repo_root}/.git/just-tmp" \
  git commit --only -m "docs(readme): auto-translate README.zh-cn.md" -- README.zh-cn.md
