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
- Force progress/status output for non-TTY stderr: `cat article.txt | hymt -t fr --progress`
- Skip interactive checks: `hymt -f input.txt -t ja --yes`
- If the source text matches a subcommand name, disambiguate with `--`: `hymt -t zh -- "config"`
- Without an explicit `-t/--target`, hymt routes between `[language].primary` (default `zh`) and `[language].secondary` (default `en`): mostly-primary input targets secondary, otherwise it targets primary.
- Mixed-language documents use smart partial translation when language detection is available: target-language paragraphs are kept, non-target paragraphs are translated, and fenced code blocks are always preserved.

## Batch translate directories

- Preview a directory without writing files: `hymt batch ./docs -t zh`
- Include subdirectories in the preview: `hymt batch ./docs -t zh --recursive`
- Translate and write outputs: `hymt batch ./docs -t zh --write`
- Skip confirmation for automation: `hymt batch ./docs -t zh --write --yes`
- Write to a separate tree while preserving source-relative paths: `hymt batch ./docs -t zh --write --output-dir translated-docs`
- Batch mode scans the top-level directory by default; add `--recursive` to descend into subdirectories. It follows symlinks and selects `.txt` and `.md` files.
- Output names are `{stem}.{target}.{ext}` using the effective target; with default routing, English files write `README.zh.md` and mostly-primary files write `README.en.md`.
- Files whose resolved output path would leave the scan root or `--output-dir` are skipped with a stderr warning.
- Batch target names accept only ASCII letters, digits, and hyphens; dots, path separators, and other characters are rejected.
- Files already detected as target-language documents are skipped. Mixed-language files keep target-language paragraphs and translate the remaining paragraphs.
- Batch planning progress is written to stderr before the preview, including scanned file count, per-file analysis, and selected/skipped totals.
- The preview lists every selected file with `full`, `partial`, or `none` segment-cache status, cached segment counts, per-file ETA, and total ETA.
- `--write` writes outputs even for fully cached files, so deleted output files can be regenerated from cache.
- Batch progress and per-file translation progress are written to stderr in the standard `[done/total] XX.XX% | elapsed ... | eta ... | NN.NN tok/s` format.

## Translate Markdown docs

- Translate one Markdown file to Simplified Chinese: `hymt translate-doc README.md`
- Explicit output path: `hymt translate-doc README.md -t zh -o README.zh-cn.md`
- Translate to another language: `hymt translate-doc README.md -t ja`
- Translate a documentation tree: `hymt translate-doc docs/ --recursive`
- Keep a file in sync while editing: `hymt translate-doc README.md --watch`
- `translate-doc` only accepts Markdown input, normalizes effective `zh` outputs to `.zh-cn.md`, and falls back to polling when `watchfiles` is unavailable.
- Watch-mode retries are bounded by `[translation].max_retranslation_retries`, and changed files reuse cached segments instead of retranslating the whole document from scratch.

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
- `hymt batch [DIRECTORY] -t <lang> [--recursive] [--write] [--output-dir <dir>] [--yes] [template options...]`
- `hymt history`
- `hymt history --all`
- `hymt history --stats`
- `hymt history --clear`
- `hymt recall`
- `hymt recall -n <N>`
- `hymt recall --list`
- `hymt translate-doc <FILE|DIR> -t <lang> [--recursive] [--watch] [--output-dir <dir>] [template options...]`
- `hymt config show`
- `hymt config path`
- `hymt config edit`
- `hymt tokenizer download [--force]`

Related skills cover command/documentation translation:

- `hymt exec -- <command> [args...]`, `hymt exec install`, and `hymt exec precache` are documented in `skills/exec/SKILL.md`.
- `hymt man [--original] [--refresh] <page>` is documented in `skills/man/SKILL.md`.
- `hymt info [--original] [--refresh] <topic>` is documented in `skills/info/SKILL.md`.
- `hymt translate-doc ...` is documented in `skills/translate-doc/SKILL.md`.

## Behavior

- Translation output goes to stdout; progress and status go to stderr when stderr is a TTY, or when direct translation uses `--progress`.
- Stdout translations end with a trailing newline.
- Explicit `-t/--target` disables default language routing and uses the requested target for prompts, cache/history keys, and output suffixes.
- When optional language detection support is installed, mixed-language runs show a per-paragraph plan and prompt: `X of Y paragraphs are already in <target_lang>. Translate only the remaining Z paragraphs? (y/n/all)`. `y` keeps target-language paragraphs, `all` translates every non-code paragraph, and `n` cancels.
- `--yes` and non-interactive stdin auto-select partial translation for mixed-language input.
- If all detected paragraphs are already in the target language, interactive runs still ask whether to translate anyway.
- Fenced code blocks are excluded from language detection and translation in every mode.
- `[translation].stream = true` is the default. `--stream`/`--no-stream` override config for translation runs.
- Streaming runs send tokens to stdout as the endpoint produces them; non-streaming runs buffer output until completion.
- Progress is written to stderr as `[done/total] XX.XX% | elapsed 2m47s | eta 1m23s | NN.NN tok/s` for TTY stderr or explicit `--progress`.
- Identical source segments, target language, template type, and template-specific options reuse cached segment translations.
- `translate-doc` persists each completed segment immediately, so watch-mode retries and interrupted runs can resume from the segment cache.
- After a completed interactive translation, if actual runtime diverges from the estimate by `[timing].divergence_threshold` (default `2.0`), `hymt` can prompt to file a GitHub timing-data issue.
- Config lives at `~/.config/hymt/config.toml`; `[language].primary` and `[language].secondary` configure default routing.
- `translate-doc --watch` also reads `[translation].max_retranslation_retries` from that config.
- The tokenizer is cached at `~/.cache/hymt/tokenizer/tokenizer.json`.
- `hymt estimate` and translation commands auto-download the tokenizer on first use when the `tokenizers` dependency is available.
- On Android/Termux installs, `hymt` uses approximate token counting for segmentation because Rust tokenizer wheels are unavailable.
