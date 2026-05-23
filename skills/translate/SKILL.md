---
name: translate
description: "Use when translating text or files through hymt, estimating segment count/runtime, recalling previous outputs, or managing hymt config/tokenizer state."
---

# Translate

Use `hymt` when an agent should drive the local Hy-MT2 CLI instead of hand-writing prompts.

## Translate text

- Positional text: `hymt "Hello world" -t zh`
- File input: `hymt -f input.txt -t ja -o output.txt`
- Stdin pipeline: `cat article.txt | hymt -t fr`
- Force or disable streaming: `hymt -f input.txt -t ja --stream` / `hymt -f input.txt -t ja --no-stream`
- Skip interactive checks: `hymt -f input.txt -t ja --yes`
- If the source text matches a subcommand name, disambiguate with `--`: `hymt -t zh -- "config"`
- Mixed-language documents use smart partial translation when language detection is available: target-language paragraphs are kept, non-target paragraphs are translated, and fenced code blocks are always preserved.

## Template types

`--type` accepts:

- `default`
- `terminology`
- `style`
- `personalization`
- `delimiters`
- `structured`
- `context`

Template-specific options:

- `--terms source=target` (repeatable) for `terminology`
- `--style TEXT` for `style`
- `--instruction TEXT` (repeatable) for `personalization`
- `--format TEXT` for `structured`
- `--context TEXT` for `context`

## Utility commands

- `hymt estimate -t <lang> [--file <path>] [template options...]`
- `hymt history`
- `hymt history --all`
- `hymt history --stats`
- `hymt history --clear`
- `hymt recall`
- `hymt recall -n <N>`
- `hymt recall --list`
- `hymt config show`
- `hymt config path`
- `hymt config edit`
- `hymt tokenizer download [--force]`

## Behavior

- Translation output goes to stdout; progress and status go to stderr.
- Stdout translations end with a trailing newline.
- When optional language detection support is installed, mixed-language runs show a per-paragraph plan and prompt: `X of Y paragraphs are already in <target_lang>. Translate only the remaining Z paragraphs? (y/n/all)`. `y` keeps target-language paragraphs, `all` translates every non-code paragraph, and `n` cancels.
- `--yes` and non-interactive stdin auto-select partial translation for mixed-language input.
- If all detected paragraphs are already in the target language, interactive runs still ask whether to translate anyway.
- Fenced code blocks are excluded from language detection and translation in every mode.
- `[translation].stream = true` is the default. `--stream`/`--no-stream` override config for translation runs.
- Streaming runs send tokens to stdout as the endpoint produces them; non-streaming runs buffer output until completion.
- Progress is written to stderr as `[done/total] XX.XX% | elapsed 2m47s | eta 1m23s | NN.NN tok/s`.
- Identical source segments, target language, template type, and template-specific options reuse cached segment translations.
- After a completed interactive translation, if actual runtime diverges from the estimate by `[timing].divergence_threshold` (default `2.0`), `hymt` can prompt to file a GitHub timing-data issue.
- Config lives at `~/.config/hymt/config.toml`.
- The tokenizer is cached at `~/.cache/hymt/tokenizer/tokenizer.json`.
- `hymt estimate` and translation commands auto-download the tokenizer on first use when the `tokenizers` dependency is available.
- On Android/Termux installs, `hymt` uses approximate token counting for segmentation because Rust tokenizer wheels are unavailable.
