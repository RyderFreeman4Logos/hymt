[中文版](README.zh-cn.md)

# hymt

![Python](https://img.shields.io/badge/python-3.11%2B-blue)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Model](https://img.shields.io/badge/model-Hy--MT2-orange)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Termux-lightgrey)

Hy-MT2 as a practical CLI: tokenizer-aware segmentation, per-segment cache reuse, pipe-safe token streaming, mixed-language document handling, batch translation, command-output translation, and hot-reloadable config.

`hymt` is designed for people who translate real terminal and Markdown workflows, not just one-off strings. It keeps progress on `stderr`, translation payloads on `stdout`, records history for ETA estimation, and can auto-file timing-divergence issues when the model behaves far outside historical expectations.

## Why hymt

- Translate positional text, stdin, or files with one command.
- Segment long inputs against the Hy-MT2 tokenizer instead of splitting blindly.
- Reuse cached translations at the segment level, so repeated paragraphs become nearly instant.
- Stream tokens by default, which keeps `| less`, `| bat`, and `| tee` workflows responsive.
- Detect mixed-language Markdown and translate only the non-target paragraphs while preserving fenced code blocks.
- Batch entire directory trees and preview cache status plus ETA before writing.
- Wrap arbitrary shell commands with `hymt exec`, or browse translated `man` and `info` pages.
- Recall previous outputs and inspect translation history with throughput statistics.
- Keep bilingual Markdown in sync with `hymt translate-doc`.

## Install

### Quick install with `uv`

```bash
uv tool install .
```

If you want optional language detection support in a local editable install:

```bash
uv pip install --system -e ".[detect]"
mise reshim
```

That gives you:

- `langdetect` for mixed-language partial translation.

### Termux / Android

Android builds skip the Rust `tokenizers` dependency, so `hymt` automatically falls back to approximate token counting. Translation still works; segmentation is just less precise than the Linux tokenizer-backed path.

```bash
uv pip install --system -e ".[detect]"
mise reshim
```

## Configure the endpoint

On first use, `hymt` creates `~/.config/hymt/config.toml`. A typical setup looks like:

```toml
[endpoint]
url = "http://100.78.159.38:8401/v1"
api_key = ""
model = ""

[translation]
context_window = 65536
max_output_tokens = 4096
concurrency = 8
stream = true
config_version = 1
timeout = 600

[completeness]
max_retries = 2

[timing]
divergence_threshold = 2.0
```

The config is hot-reloadable. Long-running workflows pick up edits without restarting the process.

## Quick start

### Translate text, stdin, or a file

```bash
hymt "Hello world" -t zh
printf 'Release notes go here.\n' | hymt -t ja
hymt -f CHANGELOG.md -t fr -o CHANGELOG.fr.md
```

### Keep streaming friendly

Streaming is enabled by default. That means you can keep normal shell pipes:

```bash
hymt -f article.md -t zh | less
hymt -f notes.txt -t ja | bat -l markdown
hymt -f report.md -t zh | tee report.zh.preview.md
```

Use `--no-stream` if you need a fully buffered response.

### Mixed-language docs stay readable

When optional language detection is installed, `hymt` keeps paragraphs that are already in the target language, translates the rest, and always preserves fenced code blocks.

That makes it practical for:

- bilingual READMEs
- design notes with copied shell output
- API docs that mix English code with Chinese commentary

## Smart segmentation and cache reuse

`hymt` plans each translation against the Hy-MT2 tokenizer and the selected prompt template. Each translated segment is cached by:

- segment content hash
- target language
- template type
- template options

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
- Completeness retry count is controlled by `[completeness].max_retries`.

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

## Timing divergence auto-issue filing

After an interactive translation, `hymt` compares actual runtime with historical estimates. When the run diverges beyond `[timing].divergence_threshold`, it can prompt to file a GitHub issue containing:

- token counts
- segment counts
- throughput stats
- config version
- model metadata

That makes it easier to track regressions in server settings, concurrency, or prompt behavior.

## Remote Hy-MT2 over Tailscale

This repo includes two sample systemd user services under [`services/`](services):

- `hy-mt2-quality.service`: Q6_K, single slot, Q8 KV, 16K context
- `hy-mt2-throughput.service`: Q4_K_M, 8 slots, Q4 KV, 64K context

Both bind to the Tailscale interface only (`100.78.159.38:8401`), not `0.0.0.0`. Point your `endpoint.url` at that address when you want to use a remote Hy-MT2 host across your tailnet.

## Architecture

- `src/hymt/config.py`: hot-reloadable TOML config
- `src/hymt/segment.py`: tokenizer-backed token counting and segmentation
- `src/hymt/client.py`: async OpenAI-compatible translation client with retry logic
- `src/hymt/translate.py`: core translate pipeline, cache lookup, streaming, progress, timing history
- `src/hymt/history.py`: SQLite task history, recall, and ETA statistics
- `src/hymt/batch.py`: directory planning, cache preview, and batch writes
- `src/hymt/doc_translate.py`: Markdown-focused translation workflow
- `src/hymt/docs.py`: translated `man` and `info`
- `src/hymt/exec_wrapper.py`: command wrapper and translated post-run output
- `src/hymt/cli.py`: Click CLI entry point

## Development

Run the full local quality gate with:

```bash
env JUST_TEMPDIR=$PWD/.git/just-tmp just pre-commit
```

If you want README bilingual sync in automation, see the `doc-translate-sync` recipe and the `post-commit` hook in `lefthook.yml`.
