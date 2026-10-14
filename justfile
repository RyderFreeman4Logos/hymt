# Justfile for hymt (Rust)
# Keep local acceptance gates enforced; never bypass commit or push hooks.

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
    '

# Run once after committing, on a clean HEAD; prints the private gate receipt path.
pre-push-gate target="head":
    python3 scripts/hooks/local-gates.py run {{target}}

# Fixed aggregate owned by the receipt producer; alone this emits no receipt.
_full-gate:
    #!/usr/bin/env bash
    set -euo pipefail
    timeout --kill-after=5 {{_timeout}} bash -c '
        set -euo pipefail
        just fmt
        just lint
        just test
        just check
        just check-no-telegram
        just test-cli-no-telegram
        just shell-syntax
        just shellcheck
        just service-verify
        just toml-parse
    '

fmt:
    {{_io_prefix}} cargo fmt --all -- --check

lint:
    {{_io_prefix}} cargo clippy --workspace --locked -- -D warnings

check:
    {{_io_prefix}} cargo check --workspace --locked

test:
    {{_io_prefix}} cargo test --workspace --locked
    just test-inference-cache

test-inference-cache:
    python3 scripts/hooks/local-gates.py test-inference-cache

# One focused test. Usage: just test-one hymt-translate omitted_link
test-one package test:
    {{_io_prefix}} cargo test -p {{package}} --locked {{test}} -- --exact

# Focused trust-boundary and real hook/transport fixtures.
test-local-gates:
    {{_io_prefix}} cargo test -p hymt-cli --locked --test local_gates -- --nocapture

# Release binary used for acceptance. Same idle prefix and cargo config as tests.
release:
    {{_io_prefix}} cargo build --release --bin hymt

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
    toolchain=()
    if [[ -n "${RUSTUP_TOOLCHAIN:-}" ]]; then
        toolchain=("+${RUSTUP_TOOLCHAIN}")
    fi
    {{_io_prefix}} cargo "${toolchain[@]}" install --path "{{_repo_root}}/crates/hymt-cli" --locked --force
    echo "hymt (Rust) installed from {{_repo_root}}"

# Install the shared inference cache independently of the Rust CLI.
install-inference-cache:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "$HOME/.local/share/hymt" "$HOME/.config/systemd/user"
    script_tmp="$(mktemp "$HOME/.local/share/hymt/.inference-cache.XXXXXXXX")"
    trap '[[ ! -e "$script_tmp" ]] || unlink "$script_tmp"; [[ ! -e "${unit_tmp:-}" ]] || unlink "$unit_tmp"' EXIT
    unit_tmp="$(mktemp "$HOME/.config/systemd/user/.hy-mt2-cache.XXXXXXXX")"
    install -m 600 "{{_repo_root}}/scripts/hymt-inference-cache.py" "$script_tmp"
    install -m 600 "{{_repo_root}}/services/hy-mt2-cache.service" "$unit_tmp"
    mv -f -- "$script_tmp" "$HOME/.local/share/hymt/inference-cache.py"
    mv -f -- "$unit_tmp" "$HOME/.config/systemd/user/hy-mt2-cache.service"
    systemctl --user daemon-reload
    systemctl --user try-restart hy-mt2-cache.service

# Install without the default `telegram` feature (drops Bot API deps).
install-no-telegram:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{_repo_root}}"
    if command -v uv >/dev/null 2>&1; then
        uv tool uninstall hymt 2>/dev/null || true
    fi
    toolchain=()
    if [[ -n "${RUSTUP_TOOLCHAIN:-}" ]]; then
        toolchain=("+${RUSTUP_TOOLCHAIN}")
    fi
    {{_io_prefix}} cargo "${toolchain[@]}" install --path "{{_repo_root}}/crates/hymt-cli" --locked --no-default-features --force
    echo "hymt (Rust, no telegram) installed from {{_repo_root}}"

# Compile-check CLI with telegram feature disabled.
check-no-telegram:
    {{_io_prefix}} cargo check -p hymt-cli --locked --no-default-features

# Test the CLI without the optional Telegram feature.
test-cli-no-telegram:
    {{_io_prefix}} cargo test -p hymt-cli --locked --no-default-features

# Check every tracked shell file, one bash parse per file.
shell-syntax:
    python3 scripts/hooks/local-gates.py shell-syntax

shellcheck:
    python3 scripts/hooks/local-gates.py shellcheck

service-verify:
    python3 scripts/hooks/local-gates.py service-verify

toml-parse:
    python3 scripts/hooks/local-gates.py toml-parse

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
