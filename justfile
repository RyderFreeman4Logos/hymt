# Justfile for hymt (Python)
# AI AGENT: Do NOT modify this file or use `git commit -n`/`--no-verify` to bypass pre-commit.

set shell := ["bash", "-c"]

_repo_root := `git rev-parse --show-toplevel`
_timeout := "3000"
_venv := _repo_root / ".venv"
_python := _venv / "bin/python"

default: pre-commit

# ==============================================================================
# Core Workflow
# ==============================================================================

pre-commit:
    #!/usr/bin/env bash
    set -euo pipefail
    timeout {{_timeout}} bash -c '
        set -euo pipefail
        just lint
        just typecheck
        just test
    '

lint:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v ruff >/dev/null 2>&1; then
        echo "SKIP lint: ruff not installed"
        exit 0
    fi
    ruff check {{_repo_root}}/src {{_repo_root}}/tests --fix
    ruff format {{_repo_root}}/src {{_repo_root}}/tests
    git diff --name-only -- '*.py' | xargs -r git add

typecheck:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v mypy >/dev/null 2>&1; then
        echo "SKIP typecheck: mypy not installed"
        exit 0
    fi
    mypy {{_repo_root}}/src --ignore-missing-imports

test:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -d "{{_venv}}" ]; then
        echo "SKIP test: no .venv"
        exit 0
    fi
    {{_python}} -m pytest {{_repo_root}}/tests -q

# ==============================================================================
# Individual commands
# ==============================================================================

fmt:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v ruff >/dev/null 2>&1; then
        echo "SKIP fmt: ruff not installed"
        exit 0
    fi
    ruff format {{_repo_root}}/src {{_repo_root}}/tests
    git diff --name-only -- '*.py' | xargs -r git add

review:
    @echo "=== Staged changes ==="
    git diff --cached --stat
    @echo ""
    @echo "=== Unstaged changes ==="
    git diff --stat

install-hooks:
    @git config --unset core.hooksPath 2>/dev/null || true
    lefthook install
    @echo "Lefthook hooks installed."

install:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{_repo_root}}"
    if command -v mise >/dev/null 2>&1; then
        mise x -- uv tool install --force --reinstall "{{_repo_root}}"
    elif command -v uv >/dev/null 2>&1; then
        uv tool install --force --reinstall "{{_repo_root}}"
    else
        echo "ERROR: mise or uv required" >&2
        exit 1
    fi
    echo "hymt installed from {{_repo_root}}"

doc-translate-sync:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{_repo_root}}"
    bash scripts/hooks/translate-readme-zh.sh
    if [ -z "$(git status --porcelain -- README.zh-cn.md)" ]; then
        echo "README.zh-cn.md already in sync."
        exit 0
    fi
    git add README.zh-cn.md
    echo "Staged README.zh-cn.md"
