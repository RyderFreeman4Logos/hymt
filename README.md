[中文版](README.zh-cn.md)

# hymt

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Model](https://img.shields.io/badge/model-Hy--MT2-orange)
![Platform](https://img.shields.io/badge/platform-Linux-lightgrey)

Hy-MT2 as a practical Rust CLI: tokenizer-aware segmentation, per-segment cache reuse, pipe-safe token streaming, Markdown-aware document translation, batch translation, command-output translation, and hot-reloadable config.

`hymt` is designed for people who translate real terminal and Markdown workflows, not just one-off strings. It keeps progress on `stderr`, translation payloads on `stdout`, records history for ETA estimation, and can auto-file timing-divergence issues when the model behaves far outside historical expectations.

## Why hymt

- Translate positional text, stdin, or files with one command.
- Segment long inputs against the Hy-MT2 tokenizer instead of splitting blindly.
- Reuse cached translations at the segment level, so repeated content becomes nearly instant.
- Stream tokens by default, which keeps `| less`, `| bat`, and `| tee` workflows responsive.
- Segment Markdown with structure-aware boundaries; see the mixed-language limitation below.
- Batch entire directory trees and preview cache status plus ETA before writing.
- Wrap arbitrary shell commands with `hymt exec`, or browse translated `man` and `info` pages.
- Recall previous outputs and inspect translation history with throughput statistics.
- Keep bilingual Markdown in sync with `hymt translate-doc`.
- Optional Telegram bot (`hymt telegram`) for private multi-owner claim and group Chinese↔English translation.

## Install

### Install

```bash
just install
```

The binary enables the `telegram` cargo feature by default. To build without Telegram Bot API dependencies:

```bash
just install-no-telegram
```

The documented and tested installation path is the Rust workspace on Linux x86_64.

## Configure the endpoint

On first use, `hymt` creates `~/.config/hymt/config.toml`. A typical setup looks like:

```toml
[endpoint]
url = "http://100.78.159.38:8401/v1"
api_key = ""
model = ""
# Select one supported, tested Hy-MT2 profile, or use "generic" for an unprofiled endpoint.
profile = "hy_mt2_7b"

[backend]
# `llama-server -c` is the service-wide allocation. The throughput unit uses
# 65,536 total tokens across 8 slots; the quality unit uses 24,576 across 3.
total_context = 65536
parallel_slots = 8
# Optional: omit to derive total_context / parallel_slots. Set this explicitly
# when the backend guarantees a lower per-request limit.
per_request_context = 8192

[translation]
max_output_tokens = 4096
max_source_tokens_per_segment = 1024
concurrency = 8 # use 3 with hy-mt2-quality.service
stream = true
config_version = 1
timeout = 600
# first_chunk_priority = false
# debug_chunk_timing = false
# For Chinese-family targets, preserve confidently target-language paragraphs.
language_detection = true
# Override detection and submit every non-code paragraph.
force_translate_all = false

[inference]
# The client sends these OpenAI-compatible request fields. They match the
# supplied service profiles; keep client overrides aligned with the endpoint.
temperature = 0.7
top_p = 0.6
top_k = 20
repetition_penalty = 1.05

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 2
# When false (default), top-level text/file/stdin exits non-zero after writing best
# attempt if any segment exhausted completeness retries. Set true (or pass
# --warn-only-completeness) to keep exit 0 with warnings only.
warn_only = false

[timing]
divergence_threshold = 2.0
```

The config is hot-reloadable except for `[endpoint].profile`, which is pinned at startup (see [Model profile](#model-profile-endpointprofile)). Long-running workflows pick up other edits without restarting the process.

## Model profile (`[endpoint].profile`)

Set `[endpoint].profile` explicitly for a Hy-MT2 endpoint. The recognized values and their coverage are:

| Value | Coverage |
|---|---|
| `hy_mt2_1_8b` | Tested Hy-MT2 1.8B profile with a pinned upstream tokenizer source and profile generation defaults. |
| `hy_mt2_7b` | Tested Hy-MT2 7B profile with a pinned upstream tokenizer source and profile generation defaults. |
| `hy_mt2_30b_a3b` | Tested Hy-MT2 30B-A3B profile with a pinned upstream tokenizer source and profile generation defaults. |
| `generic` (or omitted) | Unprofiled mode: no tested Hy-MT2 tokenizer or generation-default coverage. |

The profile is read and **pinned at process startup**. Other config values remain hot-reloadable, but changing `[endpoint].profile` on disk is ignored by the running session; restart `hymt` to use a different profile. Segment-cache keys and translation-history records retain the canonical profile ID, so results are not shared between profiles.

### Telegram bot (`[telegram]`)

Default config includes a disabled Telegram section:

```toml
[telegram]
enabled = false
bot_token = ""          # or set HYMT_TELEGRAM_BOT_TOKEN
claim_password = ""     # auto-generated on first `hymt telegram` if empty
owners = []             # private chat ids after claim
groups = []             # group chat ids when mode = "groups"
mode = "owners"         # "owners" | "groups"
```

1. Create a bot with [@BotFather](https://t.me/BotFather), set `bot_token` (or `HYMT_TELEGRAM_BOT_TOKEN`).
2. Set `enabled = true`.
3. Run `hymt telegram` (long-poll until Ctrl+C). On first run, hymt generates a claim password, stores it in config, and prints it once.
4. In a private chat with the bot, send the claim password (or `/claim <password>`) to become an owner. Multiple owners are supported.
5. Authorized owners (and members of groups listed in `groups` when `mode = "groups"`) receive automatic Chinese↔English translation of text messages. Unauthorized chats get a short denial.
6. Regenerate the claim password with `hymt telegram --regenerate-claim-password` (prints the new value once).

Secrets (`bot_token`, `claim_password`) are not re-printed on every run.

## Quick start

### Translate text, stdin, or a file

```bash
hymt "Hello world" -t zh
printf 'Release notes go here.\n' | hymt -t ja
hymt -f CHANGELOG.md -t fr -o CHANGELOG.fr.md
```

### Target language codes

All prompt construction, validation, detection, output filenames, CLI estimates, and Telegram routing use one canonical registry. Supported canonical codes are:

`zh`, `zh-Hant`, `en`, `fr`, `pt`, `es`, `ja`, `tr`, `ru`, `ar`, `ko`, `th`, `it`, `de`, `vi`, `ms`, `id`, `tl`, `hi`, `pl`, `cs`, `nl`, `km`, `my`, `fa`, `gu`, `ur`, `te`, `mr`, `he`, `bn`, `ta`, `uk`, `bo`, `kk`, `mn`, `ug`, and `yue`.

Codes are case-insensitive and normalize `_` to `-`: `zh-CN`/`zh_CN` resolve to `zh`, while `zh-TW`/`zh_Hant` resolve to `zh-Hant`. `zh`, `zh-Hant`, and `yue` share Chinese-family CJK handling; the Hy-MT2 profiles use this same registry for every supported target.

### Keep streaming friendly

Streaming is enabled by default. That means you can keep normal shell pipes:

```bash
hymt -f article.md -t zh | less
hymt -f notes.txt -t ja | bat -l markdown
hymt -f report.md -t zh | tee report.zh.preview.md
```

Use `--no-stream` if you need a fully buffered response.

Force concurrency for one run with `--concurrency N` (overrides `[translation].concurrency`). Use `--debug-chunk-timing` (or `HYMT_DEBUG_CHUNK_TIMING=1`) to print per-chunk queue/request/first-token/complete timings on stderr while diagnosing multi-segment stalls.

### Mixed-language document planning

For Chinese-family targets (`zh`, `zh-Hant`, and `yue`), hymt plans a document paragraph by paragraph before it segments requests. With the default `[translation].language_detection = true`, a paragraph is preserved when its CJK-character ratio is over 60% and it has at least four analyzed non-whitespace characters. Its original UTF-8 bytes are carried through reconstruction unchanged; other paragraphs are sent to the model.

Markdown headings, list items, blockquotes, and table rows use the same paragraph rule. Fenced code blocks and leading YAML frontmatter are always preserved. Very short, code-like, or otherwise ambiguous snippets are translated rather than classified as already-target-language. `--plan` for text, stdin, and file input prints each paragraph's detection metadata, including `is_target_language` and `should_translate`.

Use either a one-run override or configuration to translate every non-code paragraph:

```bash
hymt --force-translate-all -l zh "English text\n\n已有中文段落"
hymt --no-language-detection -l zh -f article.md
```

```toml
[translation]
language_detection = true      # default: use CJK detection for Chinese-family targets
force_translate_all = false    # default: false; set true to translate all non-code paragraphs
```

`--force-translate-all`, `--no-language-detection`, `force_translate_all = true`, and `language_detection = false` all select the translate-all policy. An explicit `-l/--lang` chooses the target but does **not** disable preservation. Detection is intentionally CJK-only: for non-Chinese targets, hymt translates all non-code paragraphs rather than claiming general multilingual detection.

## Smart segmentation and cache reuse

`hymt` plans each translation against the Hy-MT2 tokenizer and the selected prompt template. Each translated segment is currently cached by:

- segment content hash
- target language
- template type
- template options
- `profile_id` (canonical profile ID)

Profile isolation is therefore provided. The segment-cache key does **not** yet include endpoint/model identity, tokenizer revision, quantization or backend build, or inference sampling settings (see #115). Changes to those settings can therefore reuse entries from an older inference profile; `config_version` is recorded in task history, not the segment-cache key. Inference fingerprinting is required before those settings are isolated automatically.

That enables:

- fast retries after interrupted runs
- near-instant rewrites when only a few paragraphs changed
- cache reuse across normal translation, batch translation, doc translation, and translated manual pages

Progress is always reported on `stderr` in the same format:

```text
[done/total] XX.XX% | elapsed Xm Ys | eta Xm Ys | NN.NN tok/s
```

## Translate Markdown docs

`translate-doc` is the workflow-oriented command for bilingual Markdown trees.

```bash
hymt translate-doc README.md
hymt translate-doc README.md -t ja
hymt translate-doc README.md -t zh -o README.zh-cn.md
hymt translate-doc docs/ --recursive
```

Behavior:

- Default target is `zh`, and Markdown outputs normalize to `.zh-cn.md`.
- Directory mode translates Markdown files and preserves relative paths when `--output-dir` is used.
- Completeness validation is a set of fast truncation/structure heuristics: minimum character ratio, paragraph retention, and Markdown heading preservation. It can flag likely truncation or structural loss; it is not proof that a translation is semantically correct.
- Failed segments retry up to `[completeness].max_retries`; the same value applies to normal, streaming, batch, and `translate-doc` segment validation. After retries are exhausted, `hymt` still writes the best-effort output and emits `completeness_degraded_segments=…` on stderr. Top-level text/file/stdin commands, including their streaming form, then exit non-zero so scripts detect degraded results; pass `--warn-only-completeness` or set `[completeness].warn_only = true` to keep exit 0 with warnings only. `batch`, `translate-doc`, and `exec` report the same stderr marker by default but do not fail the whole job for degraded segments.
- Source segments are also bounded by the expansion/context budget and `[translation].max_source_tokens_per_segment` (default `1024`, `0` disables).

## Batch translate directory trees

Use `batch` when you want a preview-first workflow across `.md` and `.txt` files:

```bash
hymt batch docs -t zh
hymt batch docs -t zh --write --yes
hymt batch docs -t zh --write --output-dir translated-docs
```

Batch preview reports:

- selected vs skipped files
- per-file cache status: `full`, `partial`, or `none`
- cached segment counts
- per-file ETA
- total ETA

## Translate command output and manuals

### Wrap terminal commands

```bash
hymt exec -- cargo test
hymt exec -- git status
hymt exec precache --recursive
```

`hymt exec` preserves the original command output while adding translated output afterward. It is useful for unfamiliar CLIs, build failures, and long help text.

### Read translated `man` and `info`

```bash
hymt man git-rebase
hymt man --original git-rebase
hymt info coreutils
hymt info --refresh bash
```

## Recall, history, and ETA estimation

Translation history is stored in SQLite at `~/.local/share/hymt/history.db`.

Useful commands:

```bash
hymt history
hymt history --stats
hymt recall
hymt recall --list
hymt estimate 10000 -l zh
```

History powers:

- recent-output recall
- throughput statistics
- median / percentile ETA estimation
- progress bars that reflect observed token throughput

## Telegram bot

```bash
# after configuring [telegram] and enabling it
hymt telegram
hymt telegram --regenerate-claim-password
```

See the `[telegram]` config section above for claim ownership and group mode.

## Timing divergence auto-issue filing

After an interactive translation, `hymt` compares actual runtime with historical estimates. When the run diverges beyond `[timing].divergence_threshold`, it can prompt to file a GitHub issue containing:

- token counts
- segment counts
- throughput stats
- config version
- model metadata

That makes it easier to track regressions in server settings, concurrency, or prompt behavior.

## Remote Hy-MT2 over Tailscale

This repo includes two mutually exclusive sample systemd user services under [`services/`](services). `-c` is the service-wide context pool; `--parallel` divides it across concurrent requests:

| Service | Model quantization | KV cache | Total context | Parallel slots | Approx. context per slot |
|---|---|---|---:|---:|---:|
| `hy-mt2-quality.service` | Q6_K | Q8 (`q8_0`) | 24,576 | 3 | 8,192 |
| `hy-mt2-throughput.service` | Q4_K_M | Q4 (`q4_0`) | 65,536 | 8 | 8,192 |

Both bind only to `100.78.159.38:8401` on Tailscale, not `0.0.0.0`. They use CUDA `llama-server`; the quality unit points at a persistent local build, while the throughput example pins a mise `llama-cpp/9294-cuda` build. Those absolute executable and model paths are host-specific, but a replacement backend must support the shown `llama-server` context, parallel-slot, and KV-cache flags.

Both service profiles set `--temp 0.7`, `--top-k 20`, `--top-p 0.6`, and `--repeat-penalty 1.05`. Neither unit explicitly sets llama.cpp `min-p` or repeat-history length, so those use the backend defaults. The Rust client currently sends the matching `[inference]` request fields; change them deliberately and keep client and service profiles aligned.

## Architecture

- `crates/hymt-core`: hot-reloadable TOML configuration, prompt templates, CJK language utilities, and completeness heuristics.
- `crates/hymt-segment`: Hy-MT2 tokenizer integration plus hierarchical and Markdown-aware segmentation.
- `crates/hymt-client`: asynchronous OpenAI-compatible HTTP client, retry handling, concurrency limiting, and SSE streaming.
- `crates/hymt-cache`: SQLite segment and exec caches, task history, recall, and ETA statistics.
- `crates/hymt-translate`: translation orchestration, completeness retries, batch/document workflows, and translated docs.
- `crates/hymt-cli`: the Clap `hymt` binary, command dispatch, shell-facing behavior, and optional Telegram subcommand.

## Development

Install the repository hooks once, then run the local quality gate with:

```bash
just install-hooks
just pre-commit
```

Verify the binary still builds without Telegram deps:

```bash
just check-no-telegram
```

Lefthook runs `just pre-commit` before commits and provides README translation synchronization. GitHub Actions CI runs on pull requests and pushes to `main`, covering formatting, Clippy, workspace tests/checks, the no-default-features CLI check, shell checks, service-unit validation, and TOML parsing.
