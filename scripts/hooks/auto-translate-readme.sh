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

bash "${repo_root}/scripts/hooks/translate-readme-zh.sh"
if [ -z "$(git status --porcelain -- README.zh-cn.md)" ]; then
  echo "post-commit: README.zh-cn.md already in sync." >&2
  exit 0
fi

git add README.zh-cn.md
mkdir -p "${repo_root}/.git/just-tmp"
env JUST_TEMPDIR="${repo_root}/.git/just-tmp" \
  git commit -m "docs(readme): auto-translate README.zh-cn.md"
