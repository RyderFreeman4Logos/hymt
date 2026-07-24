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
- Override concurrency for one run (overrides `[translation].concurrency`): `hymt input.txt -l ja --concurrency 4`
- Log per-chunk queue/request/first-token/complete timings on stderr: `hymt input.txt -l ja --debug-chunk-timing` (or set `HYMT_DEBUG_CHUNK_TIMING=1` / `[translation].debug_chunk_timing = true`)
- Show progress indicators: `cat article.txt | hymt -l fr --progress`
- Skip interactive checks: `hymt input.txt -l ja --yes`
- If the source text matches a subcommand name, disambiguate with `--`: `hymt -l zh -- "config"`
- Without an explicit `-l/--lang`, hymt routes between `[language].primary` (default `zh`) and `[language].secondary` (default `en`): mostly-primary input targets secondary, otherwise it targets primary.
- Mixed-language documents targeting `zh`, `zh-Hant`, or `yue` preserve only high-confidence CJK target-language paragraphs by default; code blocks and frontmatter are always preserved. Use `--force-translate-all` (or `[translation].force_translate_all = true`) to submit every non-code paragraph, or `--no-language-detection` / `[translation].language_detection = false` to disable detection for one run or configuration.

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
- Files are planned by translatable segments; a file made entirely of preserved target-language paragraphs has no model segments but is still reconstructed to its normal output path. Mixed-language files preserve high-confidence Chinese-family target paragraphs and translate the remaining paragraphs.
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
- `hymt telegram` — long-poll CN↔EN Telegram bot until Ctrl+C (default cargo feature; requires `[telegram].enabled = true` and a bot token)
- `hymt telegram --regenerate-claim-password` — rotate claim password, print once, exit

### Telegram bot

- Config section `[telegram]`: `enabled` (default `false`), `bot_token` or env `HYMT_TELEGRAM_BOT_TOKEN`, `claim_password` (auto-generated once), `owners[]`, `groups[]`, `mode` (`owners`|`groups`).
- Private chat: send the claim password (or `/claim <password>`) to become an owner; multi-owner is supported.
- Authorized chats auto-translate Chinese↔English text; unauthorized chats get a short denial.
- Build without Telegram deps: `just install-no-telegram` / `cargo install --path crates/hymt-cli --no-default-features`.

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
- Target-language paragraph preservation is CJK-only (`zh`, `zh-Hant`, `yue`): explicit `-l/--lang` selects the target but does not disable it. High-confidence target paragraphs are reconstructed byte-for-byte; non-Chinese targets translate every non-code paragraph. `--plan` reports each paragraph's detection metadata and translation decision.
- Fenced code blocks and leading YAML frontmatter are excluded from language detection and translation in every mode.
- `--stream` is the default for stdout translation. Streaming emits any cached segment-0 prefix immediately; if segment 0 is uncached, it starts only that request first, then starts remaining chunks after segment 0's first non-empty backend token. Normal stdout/stdin translation emits segment 0 tokens optimistically for low TTFT even through pipes; CLI help/usage/options-like input stays validated so completeness checks can retry before stdout is written. `--output <path>` defaults to buffered throughput mode, and `--no-stream` / `--no-streaming` keep buffered stdout behavior.
- `--concurrency N` overrides `[translation].concurrency` for the process (client semaphore). Higher values improve wall-clock throughput once first-token fan-out begins; `--concurrency 1` is strictly serial and continuous but slower overall. Remaining completed segments are flushed to stdout in document order as soon as the contiguous prefix is ready.
- `--debug-chunk-timing` (also `translation.debug_chunk_timing` or `HYMT_DEBUG_CHUNK_TIMING=1`) prints per-segment `queue_enter` / `request_start` / `first_token` / `complete` / completeness-retry markers on stderr only.
- Streaming caches completed segments and emits the final reconstructed remainder after all chunks finish, so stdout remains complete and in segment order.
- Progress is written to stderr as `[done/total] XX.XX% | elapsed 2m47s | eta 1m23s | NN.NN tok/s`.
- Identical source segments, target language, template type, and template-specific options reuse cached segment translations.
- Each segment is validated for minimum output/input character ratio, paragraph retention, and Markdown heading preservation. CLI help/usage text gets a default-prompt hint that the help output is complete source text; dense translations can pass completeness when they preserve Usage/Options structure and enough `--long-option` anchors, while generic refusal text and examples-only truncation still fail. Failed segments are retried up to `[completeness].max_retries` (default `2`), the shared completeness retry setting for normal, streaming, batch, and `translate-doc`; after retries are exhausted, `hymt` continues with the best attempt, writes the best-effort output, emits `completeness_degraded_segments=…` on stderr, and exits non-zero for top-level text/file/stdin translation so scripts detect degraded results. Pass `--warn-only-completeness` or set `[completeness].warn_only = true` to keep exit 0 with warnings only.
- Source segments are bounded by the final request/context budget and by `[translation].max_source_tokens_per_segment` (default `1024`, `0` disables). Profiled local-tokenizer plans include chat framing, prompt/context, assistant marker, completeness-retry reservation, and output reservation. Oversized fenced code/table blocks fail closed with `ProtectedBlockTooLarge` before a request is sent.
- `translate-doc` persists each completed segment immediately, so interrupted runs can resume from the segment cache.
- After a completed interactive translation, if actual runtime diverges from the estimate by `[timing].divergence_threshold` (default `2.0`), `hymt` can prompt to file a GitHub timing-data issue.
- Config lives at `~/.config/hymt/config.toml`; `[language].primary` and `[language].secondary` configure default routing, `[translation].max_source_tokens_per_segment` caps per-segment source size, and `[completeness]` configures segment validation thresholds plus `warn_only`. `[telegram]` configures the optional bot (disabled until `enabled = true`).
- The tokenizer is cached at `~/.cache/hymt/tokenizer/tokenizer.json`.
- Translation commands use the cached tokenizer with a known profile chat template when present; otherwise they visibly warn and use a conservative `2x` approximate input budget plus `64` framing tokens. Run `hymt tokenizer download` to fetch the tokenizer explicitly.
- Set `[translation].strict_token_budget = true` to reject approximate planning (including generic/unprofiled endpoints or builds without the tokenizer feature).
