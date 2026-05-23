---
name: hymt-exec
description: "Use when wrapping a terminal command with hymt exec, installing the zsh t wrapper, or pre-caching command help and manpage translations."
---

# Hymt Exec

Use `hymt exec` when an agent or user wants original command output preserved and a translated explanation shown after the command exits.

## Wrap commands

- Basic form: `hymt exec -- <command> [args...]`
- Target language: `hymt exec -t zh -- cargo build`
- Original stdout and stderr are streamed unchanged while the command runs.
- After completion, stderr is translated when textual. Stdout is translated by default, but binary, JSON, XML, YAML-like structured output, configured skip patterns, and configured skip commands are skipped.
- Translated stderr is yellow on a TTY; translated stdout is cyan on a TTY. When stdout is piped, translated stdout is written to stderr so the command's stdout stays pipe-safe.
- `Ctrl+C` during the translation phase cancels translation without changing the wrapped command's exit status.

## Zsh wrapper

- Install: `hymt exec install`
- Update existing plugin: `hymt exec install --update`
- Plugin path: `~/.local/share/hymt/hymt-exec.zsh`
- `.zshrc` gets a source line if missing.
- The plugin provides explicit opt-in wrapper `t <command> [args...]`; it does not install a `preexec` hook or auto-translate every command.
- Safety checks must all pass before `t` calls `hymt exec`: interactive shell, stdout/stderr are TTYs, no known agent env vars, no known agent ancestor process, command not blocklisted, command is not `hymt`, and not running inside a script.

## Precache

- Precache manpages and top-level `--help`: `hymt exec precache`
- Include subcommand help discovered from help output: `hymt exec precache --recursive`
- Limit manpages to a section: `hymt exec precache --section 1`
- Progress is written to stderr as `[done/total] XX.XX% | elapsed ... | eta ... | NN.NN items/s`.
- Precache uses the same user/shared exec cache and the segment cache for deduplication.

## Config

```toml
[exec]
shared_cache_path = "/usr/local/share/hymt/cache.db"
translate_stderr = true
translate_stdout = true
skip_patterns = []
skip_commands = []

[exec.plugin]
blocklist = ["hymt", "ssh", "scp"]
```
