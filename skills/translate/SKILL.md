---
name: translate
description: "Use when translating text or files through hymt, estimating segment count/runtime, recalling previous outputs, or managing hymt config/tokenizer state."
---

# Translate

Use `hymt` when an agent should drive the local Hy-MT2 CLI instead of hand-writing prompts.

## Translate text

- Positional text: `hymt "Hello world" -l zh`
- File input: `hymt input.txt -l ja`
- Stdin pipeline: `cat article.txt | hymt -l fr`
- Force or disable streaming: `hymt input.txt -l ja --stream` / `hymt input.txt -l ja --no-stream`
- Show progress indicators: `cat article.txt | hymt -l fr --progress`
- Skip interactive checks: `hymt input.txt -l ja --yes`
- If the source text matches a subcommand name, disambiguate with `--`: `hymt -l zh -- "config"`
- Without an explicit `-l/--lang`, hymt routes between `[language].primary` (default `zh`) and `[language].secondary` (default `en`): mostly-primary input targets secondary, otherwise it targets primary.
- Mixed-language documents use smart partial translation when language detection is available: target-language paragraphs are kept, non-target paragraphs are translated, and fenced code blocks are always preserved.

## Batch translate directories

- Preview a directory without writing files: `hymt batch ./docs -l zh --dry-run`
- Include subdirectories in the preview: `hymt batch ./docs -l zh --dry-run --recursive`
- Translate and write outputs: `hymt batch ./docs -l zh`
- Skip confirmation for automation: `hymt batch ./docs -l zh --yes`
- Write to a separate tree while preserving source-relative paths: `hymt batch ./docs -l zh --output-dir translated-docs`
- Batch mode scans the top-level directory by default; add `--recursive` to descend into subdirectories. It follows symlinks and selects `.txt` and `.md` files.
- Output names are `{stem}.{target}.{ext}` using the effective target; with default routing, English files write `README.zh.md` and mostly-primary files write `README.en.md`.
- Files whose resolved output path would leave the scan root or `--output-dir` are skipped with a stderr warning.
- Batch target names accept only ASCII letters, digits, and hyphens; dots, path separators, and other characters are rejected.
- Files already detected as target-language documents are skipped. Mixed-language files keep target-language paragraphs and translate the remaining paragraphs.
- Batch planning progress is written to stderr before the preview, including scanned file count, per-file analysis, and selected/skipped totals.
- The preview lists every selected file with `full`, `partial`, or `none` segment-cache status, cached segment counts, per-file ETA, and total ETA.
- `--dry-run` (or global `--plan`) shows the plan and exits without writing any files.
- Batch progress and per-file translation progress are written to stderr in the standard `[done/total] XX.XX% | elapsed ... | eta ... | NN.NN tok/s` format.

## Translate Markdown docs

- Translate one Markdown file to Simplified Chinese: `hymt translate-doc README.md`
- Explicit output path: `hymt translate-doc README.md -l zh --output README.zh-cn.md`
- Translate to another language: `hymt translate-doc README.md -l ja`
- Translate a documentation tree: `hymt translate-doc docs/ --recursive`
- `translate-doc` only accepts Markdown input and normalizes effective `zh` outputs to `.zh-cn.md`.

## Template types

`--template` accepts:

- `default`
- `terminology`
- `style`
- `personalization`
- `delimiters`
- `structured`
- `context-aware`

Template-specific options:

- `--term src=tgt` (repeatable) for `terminology`
- `--style TEXT` for `style`
- `--instructions TEXT` for `personalization`
- `--format-type TEXT` for `structured`
- `--context TEXT` for `context-aware`

## Utility commands

- `hymt estimate <SOURCE_CHARS>`
- `hymt batch DIRECTORY [-l <lang>] [--recursive] [--dry-run] [--output-dir <dir>] [--yes]`
- `hymt history`
- `hymt history stats`
- `hymt history --clear`
- `hymt history recent [N]`
- `hymt recall [<position>]`
- `hymt translate-doc <FILE|DIR> [-l <lang>] [--recursive] [--output-dir <dir>]`
- `hymt config show`
- `hymt config path`
- `hymt config edit`
- `hymt tokenizer download [--force]`

Related skills cover command/documentation translation:

- `hymt exec -- <command> [args...]`, `hymt exec install`, and `hymt exec precache` are documented in `skills/exec/SKILL.md`.
- `hymt man [--original] <page>` is documented in `skills/man/SKILL.md`.
- `hymt info [--original] <topic>` is documented in `skills/info/SKILL.md`.
- `hymt translate-doc ...` is documented in `skills/translate-doc/SKILL.md`.

## Behavior

- Translation output goes to stdout; progress and status go to stderr.
- Stdout translations end with a trailing newline.
- With piped stdin, a single positional `.` is treated as a stdin placeholder so commands like `producer | hymt .` translate the pipe instead of the literal dot.
- Explicit `-l/--lang` disables default language routing and uses the requested target for prompts, cache/history keys, and output suffixes.
- When optional language detection support is installed, mixed-language runs show a per-paragraph plan and prompt: `X of Y paragraphs are already in <target_lang>. Translate only the remaining Z paragraphs? (y/n/all)`. `y` keeps target-language paragraphs, `all` translates every non-code paragraph, and `n` cancels.
- `--yes` and non-interactive stdin auto-select partial translation for mixed-language input.
- If all detected paragraphs are already in the target language, interactive runs still ask whether to translate anyway.
- Fenced code blocks and leading YAML frontmatter are excluded from language detection and translation in every mode.
- `--stream` is the default for stdout translation. Streaming emits any cached segment-0 prefix immediately; if segment 0 is uncached, it starts only that request first, then starts remaining chunks after segment 0's first non-empty backend token. On a terminal, segment 0 tokens are shown optimistically for low TTFT, while pipe stdout buffers segment 0 until completeness passes and emits leading untranslated blocks first. `--output <path>` defaults to buffered throughput mode, and `--no-stream` / `--no-streaming` keep buffered stdout behavior.
- Streaming caches completed segments and emits the final reconstructed remainder after all chunks finish, so stdout remains complete and in segment order.
- Progress is written to stderr as `[done/total] XX.XX% | elapsed 2m47s | eta 1m23s | NN.NN tok/s`.
- Identical source segments, target language, template type, and template-specific options reuse cached segment translations.
- Each segment is validated for minimum output/input character ratio, paragraph retention, and Markdown heading preservation. CLI help/usage text gets a default-prompt hint that the help output is complete source text; dense translations can pass completeness when they preserve Usage/Options structure and enough `--long-option` anchors, while generic refusal text and examples-only truncation still fail. Failed segments are retried up to `[completeness].max_retries` (default `2`), the shared completeness retry setting for normal, streaming, batch, and `translate-doc`; after retries are exhausted, `hymt` warns and continues with the best attempt.
- `translate-doc` persists each completed segment immediately, so interrupted runs can resume from the segment cache.
- After a completed interactive translation, if actual runtime diverges from the estimate by `[timing].divergence_threshold` (default `2.0`), `hymt` can prompt to file a GitHub timing-data issue.
- Config lives at `~/.config/hymt/config.toml`; `[language].primary` and `[language].secondary` configure default routing, and `[completeness]` configures segment validation thresholds.
- The tokenizer is cached at `~/.cache/hymt/tokenizer/tokenizer.json`.
- Translation commands use the cached tokenizer when present; otherwise they use approximate token counting. Run `hymt tokenizer download` to fetch the tokenizer explicitly.
- On Android/Termux installs, `hymt` uses approximate token counting for segmentation because Rust tokenizer wheels are unavailable.
