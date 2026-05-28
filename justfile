# Justfile for hymt (Rust)
# AI AGENT: Do NOT modify this file or use `git commit -n`/`--no-verify` to bypass pre-commit.

set shell := ["bash", "-c"]

_repo_root := `git rev-parse --show-toplevel`
_timeout := "3000"

default: pre-commit

# ==============================================================================
# Core Workflow
# ==============================================================================

pre-commit:
    #!/usr/bin/env bash
    set -euo pipefail
    timeout {{_timeout}} bash -c '
        set -euo pipefail
        just fmt
        just lint
        just test
    '

fmt:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace -- -D warnings

check:
    cargo check --workspace

test:
    cargo test --workspace

# ==============================================================================
# Individual commands
# ==============================================================================

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
    if command -v uv >/dev/null 2>&1; then
        uv tool uninstall hymt 2>/dev/null || true
    fi
    cargo install --path "{{_repo_root}}/crates/hymt-cli" --force
    echo "hymt (Rust) installed from {{_repo_root}}"

# ==============================================================================
# Misc
# ==============================================================================

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
