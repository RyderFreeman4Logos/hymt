# Justfile for hymt (Rust)
# AI AGENT: Do NOT modify this file or use `git commit -n`/`--no-verify` to bypass pre-commit.

set shell := ["bash", "-c"]
# IO scheduling: run cargo at idle priority to avoid starving interactive processes
_io_prefix := "ionice -c 3 nice -n 19"


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
    {{_io_prefix}} cargo fmt --all -- --check

lint:
    {{_io_prefix}} cargo clippy --workspace -- -D warnings

check:
    {{_io_prefix}} cargo check --workspace

test:
    {{_io_prefix}} cargo test --workspace

# Complete reproducible benchmark suite. Default is deterministic mock mode;
# pass runner arguments directly, for example: just benchmark --dry-run.
benchmark *args:
    {{_io_prefix}} cargo run -p hymt-bench -- run {{args}}

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
    {{_io_prefix}} cargo install --path "{{_repo_root}}/crates/hymt-cli" --force
    echo "hymt (Rust) installed from {{_repo_root}}"

# Install without the default `telegram` feature (drops Bot API deps).
install-no-telegram:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{_repo_root}}"
    if command -v uv >/dev/null 2>&1; then
        uv tool uninstall hymt 2>/dev/null || true
    fi
    {{_io_prefix}} cargo install --path "{{_repo_root}}/crates/hymt-cli" --no-default-features --force
    echo "hymt (Rust, no telegram) installed from {{_repo_root}}"

# Compile-check CLI with telegram feature disabled.
check-no-telegram:
    {{_io_prefix}} cargo check -p hymt-cli --no-default-features

# =============================================================================
# Misc
# =============================================================================

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
