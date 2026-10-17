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
- Opt in to application translation caches when you want segment and whole-output reuse.
- Stream tokens by default, which keeps `| less`, `| bat`, and `| tee` workflows responsive.
- Segment Markdown with structure-aware boundaries; see the mixed-language limitation below.
- Batch entire directory trees and preview cache status plus ETA before writing.
- Wrap arbitrary shell commands with `hymt exec`, or browse translated `man` and `info` pages.
- Recall previous outputs and inspect translation history with throughput statistics.
- Keep bilingual Markdown in sync with `hymt translate-doc`.
- Optional Telegram bot (`hymt telegram`) for private multi-owner claim and group Chinese↔English translation, including `.txt` and `.md` documents.

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
# Choose the adapter from the server implementation, not its URL.
backend = "llama_cpp" # "llama_cpp" | "vllm" | "openai_compatible"

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
max_source_tokens_per_segment = 384
concurrency = 8 # use 3 with hy-mt2-quality.service
stream = true
config_version = 1
timeout = 600
# first_chunk_priority = false
# debug_chunk_timing = false
# Refuse planning when the final chat template cannot be tokenized locally.
# Otherwise hymt emits a warning and uses a conservative approximate budget.
strict_token_budget = false
# Except for verified local replay, refuse live work before segment-cache lookup
# or model invocation when the backend runtime is unverified or incompatible.
strict_backend_preflight = false
# For Chinese-family targets, preserve confidently target-language paragraphs.
language_detection = true
# Override detection and submit every non-code paragraph.
force_translate_all = false

[inference]
# The inference service owns sampler defaults. With no explicit override, hymt
# omits every sampler field from the JSON request, including for Hy-MT2 profiles.
# Profiles remain tokenizer/model metadata and service-deployment guidance.

[inference.override]
# A number is an explicit semantic value; "disabled" is mapped by the selected
# adapter to that backend's documented wire value.
# temperature = 0.7
# top_p = 0.6
# top_k = "disabled"
# repetition_penalty = 1.05
# min_p = 0.1
# repeat_last_n = 64 # llama.cpp only

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 2
# Retain marked complete original paragraphs after content validation retries fail.
# Set false for strict failure instead. Runtime errors and atomic JSON remain strict.
warn_only = true

[timing]
divergence_threshold = 2.0
```

The config is hot-reloadable except for `[endpoint].profile`, which is pinned at startup (see [Model profile](#model-profile-endpointprofile)). Long-running workflows pick up other edits without restarting the process.

### Backend-specific sampling (`[endpoint].backend`)

Choose `backend` explicitly from the server implementation; hymt never infers it from the endpoint URL. The generated config selects `llama_cpp`. If the key is omitted, hymt uses conservative `openai_compatible` mode and sends no nonstandard sampling extension.

| Backend | Supported override fields | Backend-specific wire behavior |
|---|---|---|
| `llama_cpp` | `temperature`, `top_p`, `top_k`, `repetition_penalty`, `min_p`, `repeat_last_n` | `repetition_penalty` is sent as `repeat_penalty`; disabled `top_k` and `repeat_last_n` are sent as `0`. |
| `vllm` | `temperature`, `top_p`, `top_k`, `repetition_penalty`, `min_p` | `repetition_penalty` is sent as `repetition_penalty`; disabled `top_k` is sent as `-1`. `repeat_last_n` is rejected. |
| `openai_compatible` | `temperature`, `top_p` | Only common chat-completions fields are sent; every nonstandard explicit override is rejected rather than guessed. |

An omitted `[inference.override]` key is always `Setting::ServerDefault`, so it is absent from the JSON request and the service applies its own configured value. This is also true for every Hy-MT2 profile: profile sampling values are service-deployment guidance and are never automatically injected into a request payload. Set a numeric override only when the client must deliberately replace the service default; `"disabled"` and numeric values are semantic configuration states, and adapters choose documented wire values rather than converting `0` and `-1` indiscriminately. Explicit overrides are included in diagnostics and the inference/cache fingerprint. Legacy scalar sampler values directly under `[inference]` remain accepted as explicit overrides for one release and emit the startup migration warning; move them under `[inference.override]`. Validation errors name the semantic value and say when no backend wire representation exists. Streaming and non-streaming requests use the same adapter policy.

The supported extension names above are limited to the documented llama.cpp server controls and vLLM OpenAI-server sampling parameters: [llama.cpp server API](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md) and [vLLM OpenAI-compatible server](https://docs.vllm.ai/en/latest/serving/openai_compatible_server/). An old `[inference].backend` key is rejected; move it to `[endpoint].backend`.

### Runtime backend preflight and inspection

Translation paths preflight the configured backend before live planning or segment-cache lookup; an explicitly enabled, validated local complete-output replay can return first. llama.cpp uses `GET /props`; vLLM and other OpenAI-compatible services use `GET /v1/models` when available. The normalized runtime state includes service build/model identity, context and slot limits, sampler defaults, and only explicitly advertised capabilities. Missing fields remain unknown rather than being inferred.

When llama.cpp `/props` reports a single ASCII punctuation `eos_token` *and* the chat template contains a `<|...|>` control marker, hymt detects a contradiction: the template's marker is the real EOS, while the metadata token is false. It resolves the false token through `/tokenize` during preflight and sends llama.cpp-only `logit_bias` `{ "<token_id>": -100 }`, preventing the false stop without suppressing the actual chat-template EOS token (such as `<|eos|>`). A single-character EOS that the chat template itself uses is left untouched.

Run `hymt backend inspect` to print configured and service-resolved values side by side. It never prints API keys, endpoint credentials, or request headers. Warnings identify context/model/profile mismatches, unexpected service sampler state, and a llama.cpp/vLLM repeat-penalty wire-key mismatch.

Normal preflight is fail-open: it warns, marks the inference fingerprint/cache identity unverified, and plans with conservative context/output limits. Set `[translation].strict_backend_preflight = true` to refuse live work before segment-cache lookup or model invocation when identity cannot be verified or a material mismatch is found. Runtime state is TTL-cached for 60 seconds and refreshed when the endpoint/backend/profile changes; a changed service identity replaces the old resolved state and fingerprint.

## Model profile (`[endpoint].profile`)

Set `[endpoint].profile` explicitly for a Hy-MT2 endpoint. The recognized values and their coverage are:

| Value | Coverage |
|---|---|
| `hy_mt2_1_8b` | Tested Hy-MT2 1.8B profile with a pinned upstream tokenizer source and service-deployment sampling guidance. |
| `hy_mt2_7b` | Tested Hy-MT2 7B profile with a pinned upstream tokenizer source and service-deployment sampling guidance. |
| `hy_mt2_30b_a3b` | Tested Hy-MT2 30B-A3B profile with a pinned upstream tokenizer source and service-deployment sampling guidance. |
| `generic` (or omitted) | Unprofiled mode: no tested Hy-MT2 tokenizer or sampling guidance. |

The profile is read and **pinned at process startup**. Other config values remain hot-reloadable, but changing `[endpoint].profile` on disk is ignored by the running session; restart `hymt` to use a different profile. Segment-cache keys and translation-history records retain the canonical profile ID, so results are not shared between profiles.

### Telegram bot (`[telegram]`)

Default config includes a disabled Telegram section:

```toml
[telegram]
enabled = false
stream = true              # progressively edit one response for multi-segment text translations
accept_documents = true      # accept .txt/.md documents
max_document_size = 1048576  # bytes; default 1 MiB
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
5. Authorized owners (and configured groups when `mode = "groups"`) get automatic Chinese↔English translation for text messages and UTF-8 `.txt`/`.md` documents. Documents over `max_document_size` or non-text documents are rejected; short translations are replied inline and longer translations are returned as a document with the original extension.
6. Set `accept_documents = false` to opt out of document handling.
7. Regenerate the claim password with `hymt telegram --regenerate-claim-password` (prints the new value once).

Secrets (`bot_token`, `claim_password`) are not re-printed on every run.

## Quick start

### Translate text, stdin, or a file

```bash
hymt "Hello world" -l zh
printf 'Release notes go here.\n' | hymt -l ja
hymt CHANGELOG.md -l fr -o CHANGELOG.fr.md
```

File output from text, stdin, file, `batch`, and `translate-doc` uses an exclusively created temporary in the destination directory, writes and flushes all bytes, closes it, then renames it into place. Failed or cancelled writes preserve the previous file and its hard-link aliases; successful replacement leaves old aliases unchanged. Valid leaf symlinks to regular files and directory symlinks remain links. Dangling links, nonregular targets, and permission errors are rejected. Existing ordinary permission bits are retained; new files follow the process umask. ACLs, extended attributes, ownership, special mode bits, power-loss durability, and concurrent path replacement are outside this guarantee. Validated cache/history written before an output error is not rolled back.

### Target language codes

All prompt construction, validation, detection, output filenames, CLI estimates, and Telegram routing use one canonical registry. Supported canonical codes are:

`zh`, `zh-Hant`, `en`, `fr`, `pt`, `es`, `ja`, `tr`, `ru`, `ar`, `ko`, `th`, `it`, `de`, `vi`, `ms`, `id`, `tl`, `hi`, `pl`, `cs`, `nl`, `km`, `my`, `fa`, `gu`, `ur`, `te`, `mr`, `he`, `bn`, `ta`, `uk`, `bo`, `kk`, `mn`, `ug`, and `yue`.

Codes are case-insensitive and normalize `_` to `-`: `zh-CN`/`zh_CN` resolve to `zh`, while `zh-TW`/`zh_Hant` resolve to `zh-Hant`. `zh`, `zh-Hant`, and `yue` share Chinese-family CJK handling; the Hy-MT2 profiles use this same registry for every supported target.

### Keep streaming friendly

Streaming is enabled by default. That means you can keep normal shell pipes:

```bash
hymt article.md -l zh | less
hymt notes.txt -l ja | bat -l markdown
hymt report.md -l zh | tee report.zh.preview.md
```

Use `--no-stream` if you need a fully buffered response.

Force concurrency for one run with `--concurrency N` (overrides `[translation].concurrency`). Use `--debug-chunk-timing` (or `HYMT_DEBUG_CHUNK_TIMING=1`) to print per-chunk queue/request/first-token/complete timings on stderr while diagnosing multi-segment stalls.

### Mixed-language document planning

For Chinese-family targets (`zh`, `zh-Hant`, and `yue`), hymt plans a document paragraph by paragraph before it segments requests. With the default `[translation].language_detection = true`, a paragraph is preserved when its CJK-character ratio is over 60% and it has at least four analyzed non-whitespace characters. Its original UTF-8 bytes are carried through reconstruction unchanged; other paragraphs are sent to the model.

Markdown headings, list items, blockquotes, and table rows use the same paragraph rule. Fenced code blocks and leading YAML frontmatter are always preserved. Very short, code-like, or otherwise ambiguous snippets are translated rather than classified as already-target-language. `--plan` for text, stdin, and file input prints each paragraph's detection metadata, including `is_target_language` and `should_translate`.

Global `--plan` supports text, stdin, file input, and `batch`; other subcommands, including `translate-doc`, reject it before config, backend, or output access. Supported planners may read backend metadata and compatible cache/history data, but do not translate or publish outputs.

Use either a one-run override or configuration to translate every non-code paragraph:

```bash
hymt --force-translate-all -l zh "English text\n\n已有中文段落"
hymt --no-language-detection -l zh article.md
```

```toml
[translation]
language_detection = true      # default: use CJK detection for Chinese-family targets
force_translate_all = false    # default: false; set true to translate all non-code paragraphs
```

`--force-translate-all`, `--no-language-detection`, `force_translate_all = true`, and `language_detection = false` all select the translate-all policy. An explicit `-l/--lang` chooses the target but does **not** disable preservation. Detection is intentionally CJK-only: for non-Chinese targets, hymt translates all non-code paragraphs rather than claiming general multilingual detection.

## Smart segmentation and cache reuse

Application segment and whole-output caches default to off. Set `[cache].enabled = true` to opt in; `--no-cache` overrides that setting for normal translation, batch, man/info, exec and Telegram commands. Disabling these caches preserves old cache entries, translation history, timing estimates and recall. The service inference cache independently reuses exact full HTTP requests/responses even when application caches are off. Explicit `--no-cache` bypasses both application and service cache reads and writes. Automatic validation retries also bypass service caching without changing sibling requests. `hymt exec precache` explicitly populates application caches even with the default setting; combining it with `--no-cache` is rejected.

With `[cache].enabled = true`, eligible completed runs also retain a bounded proof alongside the task output. Matching source, target, current rendered prompts and adapter request bytes, tokenizer, ordered ownership plan, limits and validation policy can replay locally before remote preflight, including strict preflight, when the service is unreachable. Historical runtime evidence stays inside replay validation and never authorizes new requests or cold timing samples. Missing, legacy, corrupt or changed evidence falls through to normal live behavior without emitting a tentative prefix. Initial complete runs with Verified identity qualify; degraded results, completeness retries and legacy segment-cache hits do not populate proofs. Stream and buffered request contracts remain separate. `hymt history --clear` deletes these proofs with their tasks. This validates retained output mechanically, not semantic translation quality or the current backend.


`hymt` plans each translation against the Hy-MT2 tokenizer and the selected prompt template. When application caching is enabled, translated segments are keyed by:

- segment content hash
- target language
- template type
- template options
- `profile_id` (canonical profile ID)
- verified inference fingerprint

The fingerprint includes the configured endpoint, backend and model, profile model/tokenizer sources, explicit sampling settings, and observed runtime metadata when available. Segment-cache reuse requires caching to be enabled and the inference identity to be verified; translation can continue without segment caching when identity is unverified. The fingerprint cannot attest to model bytes or quantization absent from observed metadata. After an invisible in-place model replacement, run `hymt history --clear` to remove segment entries and task history before reuse. `config_version` is recorded in task history, not the segment-cache key.

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
hymt translate-doc README.md -l ja
hymt translate-doc README.md -l zh --output README.zh-cn.md
hymt translate-doc docs/ --recursive
```

Behavior:

- Default target is `zh`, and Markdown outputs normalize to `.zh-cn.md`.
- Directory mode translates Markdown files and preserves relative paths when `--output-dir` is used.
- Completeness validation is a fast, layered truncation/structure guard — **not** translation quality estimation (QE) or proof of semantic correctness. For calibrated English/Chinese targets, density is the output/input ratio of the same `ceil(utf8_bytes / 4)` estimate the segmenter uses when its tokenizer is absent. A Unicode-scalar ratio rejects complete English-to-Chinese prose because Chinese uses fewer scalars per word. Other targets explicitly report `unverified_density` rather than silently claiming density has passed. It also checks empty/terminated responses when supplied by a caller, paragraphs, Markdown headings and fenced blocks, placeholders, URLs, and valid JSON templates. Cached segments are revalidated with the current guard before reuse.
- Ordinary paragraphs containing emphasis, inline links/images, or inline code use one complete request when the final request budget and protected-byte check permit it. Protected syntax, payload bytes and parser roles remain exact; references, math, escaped syntax, block containers and oversized paragraphs keep the existing source-owned splitting path. Complete inline paragraphs use Validated streaming even in strict mode.
- Failed segments retry up to `[completeness].max_retries` across normal, streaming, batch, and `translate-doc` paths. By default, exhausted content or Markdown validation retains the complete original paragraph unchanged and continues translating other paragraphs. A visible notice outside Markdown containers identifies retained paragraphs by source byte and body line ranges; stderr reports `completeness_status=degraded_original_retained` and `completeness_degraded_segments=…`. Source ownership is fixed before reference expansion and long-paragraph splitting: failure of any piece restores its whole original paragraph once. Retained pieces are excluded from accepted segment and exec caches; degraded documents never populate offline replay proofs or cold timing samples. Streaming with retention enabled waits for validation and emits exactly the final marked output. Set `[completeness].warn_only = false` for strict failure, or use `--warn-only-completeness` to override strict configuration. Runtime, network, authentication, configuration, and cancellation errors remain errors. These mechanical checks do not prove semantic translation accuracy.
- Repeated URLs and placeholders retain every source occurrence. Atomic JSON keeps the same object keys, array lengths, value types, and non-string scalar values; only string values may translate. JSON validation exhaustion remains an error in every mode.
- Source segments are bounded by the expansion/context budget and `[translation].max_source_tokens_per_segment` (default `384`, `0` disables). The conservative default avoids 7B models ending a long translation before their requested output limit; raise it only after validating complete output on the deployed endpoint. For a pinned Hy-MT2 profile with its tokenizer downloaded, the planner renders and counts the complete chat request (role framing, prompt/context, assistant marker, and the completeness-retry reservation) before reserving output tokens. `--plan` reports the counting source, profile/tokenizer/template identity, per-slot capacity, input/output breakdown, and any segment revisions.
- If the active profile/template or tokenizer is unavailable, hymt prints an explicit stderr warning and applies a conservative `2x` input estimate plus a `64`-token chat-framing allowance. Set `[translation].strict_token_budget = true` to reject that approximate path instead; configure `[endpoint].profile` and run `hymt tokenizer download` for local budgeting.
- Fenced code is preserved as source. When the planner excludes an oversized protected Markdown block, including a table, it warns and preserves the source bytes instead of splitting them for the model; surrounding prose can still translate. Atomic JSON that exceeds its budget is rejected before any translation request.

## Batch translate directory trees

Use `batch` for `.md` and `.txt` files. Add `--dry-run` to preview without writing; otherwise it previews and writes the selected outputs:

```bash
hymt batch docs -l zh --dry-run
hymt batch docs -l zh --yes
hymt batch docs -l zh --output-dir translated-docs
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
hymt exec precache
```

`hymt exec` preserves the original command output while adding translated output afterward. It is useful for unfamiliar CLIs, build failures, and long help text.

### Read translated `man` and `info`

```bash
hymt man git-rebase
hymt man --original git-rebase
hymt info coreutils
hymt info bash
```

## Recall, history, and ETA estimation

Translation history is stored in SQLite at `~/.local/share/hymt/history.db`.

Useful commands:

```bash
hymt history
hymt history stats
hymt recall
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

Both bind only to `100.78.159.38:8401` on Tailscale, not `0.0.0.0`. They use CUDA `llama-server`; both units use the persistent local build at `/home/obj/.local/share/llama-cpp-cuda/llama-server` and its matching library directory. Those absolute executable and model paths are host-specific, but a replacement backend must support the shown `llama-server` context, parallel-slot, and KV-cache flags.

Both service profiles explicitly set `--temp 0.7`, `--top-k 20`, `--top-p 0.6`, and `--repeat-penalty 1.05`, the Hy-MT2 deployment recommendation. They also explicitly set `--min-p 0` (disable this llama.cpp-only extension) and `--repeat-last-n 64` (a deliberate llama.cpp compatibility choice for the 1.05 repeat penalty, not an upstream Hy-MT2 recommendation). The units therefore do not inherit an accidental sampler profile from the installed llama.cpp version. hymt omits sampling fields unless `[inference.override]` explicitly replaces a value, so changing a service default changes default translations. Ordinary translation preflights llama.cpp with `GET /props` and prints relevant warnings; `hymt backend inspect` shows normalized configured and resolved sampler fields. An unavailable or older `/props` leaves identity unverified and conservative planning continues unless strict backend preflight is enabled.

### Optional shared inference cache

The separate plaintext cache stores complete raw inference responses, including usage, across clients and API/VK credentials. Its identity uses observed upstream model, build, defaults, template, and context metadata; it probes the upstream before lookup. During a classified transport outage or HTTP 502/503/504, exact populated requests can reuse the last verified same-origin active identity and epoch from the existing SQLite store. It does not replace hymt's translation validation or segment cache.

Restrict direct cache access to trusted Tailnet callers, or place it behind the existing authenticating gateway. API/VK credentials do not partition cache hits.

`just install-inference-cache` copies the script and user unit to stable paths, reloads systemd, and restarts the cache if it is active. To start it for the first time, run `systemctl --user enable --now hy-mt2-cache.service`. The supplied unit is host-specific: it listens on Tailscale `100.78.159.38:8402` and forwards to the model at `100.78.159.38:8401`. To opt a client into this service, set `[endpoint].url = "http://100.78.159.38:8402/v1"`, keep `backend = "llama_cpp"`, and set `[endpoint].model` to an actual ID or alias advertised by the upstream `/v1/models`. The example config's empty `model` omits that request field and cannot produce cache hits.

Run `python3 "$HOME/.local/share/hymt/inference-cache.py" clear --database "$HOME/.local/state/hymt/inference-cache.sqlite3"` after replacing model bytes without changing the observed metadata; clear invalidates persisted entries and in-flight admission. Clear also removes retained planning evidence. When the cache HTTP service remains reachable, it can return exact populated JSON/SSE responses while the model backend is unavailable, including after a cache-service restart. Retained `/props` and `/v1/models` carry `X-Hymt-Planning: historical` and `X-Hymt-Observed-At`; the CLI uses those historical planning limits without claiming fresh verification. One bounded exact tokenization response preserves false-EOS request bias. Strict preflight continues to require live verification. Misses and explicit bypass still need the backend. Authentication rejection, malformed metadata, TLS verification errors, and local capacity/database failures never authorize replay. Origin/model changes invalidate old evidence. Each database has a private one-byte `.guard` file and permits one cache process. Interrupted identity probes or failed invalidation block historical replay until complete online recovery; keep the guard with its database. Explicit bypass remains transparent and does not observe cache identity. The cache remains independent of hymt's translation history; this does not make an unreachable cache service available offline.

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

After committing, run the complete local acceptance gate once on the clean HEAD:

```bash
just pre-push-gate head
```

The fast commit hook checks formatting and locked workspace Clippy. The full gate adds locked workspace tests/checks, the inference cache's Python contract tests, the CLI check and tests without Telegram, every tracked shell script's syntax and ShellCheck checks, all three systemd units' static validation, and every tracked TOML file's parsing. All diagnostics remain visible; only a missing canonical persistent executable for either known model service unit is allowed on unprovisioned hosts.

Then obtain an independent whole `main...HEAD` native review with PASS and zero actionable findings. Set `HYMT_LOCAL_GATE_RECEIPT` to the gate's printed private receipt path and `HYMT_NATIVE_REVIEW_RECEIPT` to the independent review receipt described in [native review receipt format](#native-review-receipt-format). Lefthook validates their exact HEAD/tree, range, and log/report contents against the actual outgoing feature-branch update; pushing does not rerun unchanged checks. Source changes require fresh receipts. Artifacts use the private SSD task root `/mnt/ssd/mirror-rootfs/home/obj/tmp/hymt-stop-ai-20261012`; there is no hosted GitHub Actions workflow.

### Native review receipt format

The independent reviewer must perform the strongest available review of the entire `main...HEAD` range, confirm a clean checkout before and after, and write the actual report and receipt under the private SSD root above (directories `0700`, regular non-symlink files `0600`). The source author must not manufacture a PASS verdict or copy synthetic fixture evidence. Keep the checkout frozen at the reviewed HEAD through push. The gate producer writes the genuine gate receipt; it is not assembled from this example.

The following JSON is a schematic field layout, not a usable receipt. The native receipt has exactly these 13 fields, without extras or duplicates. Use actual `git symbolic-ref --short HEAD`, `git rev-parse HEAD`, `git rev-parse 'HEAD^{tree}'`, and `git merge-base main HEAD` values; `outgoing_oid` is the same HEAD. `report_sha256` is the SHA256 of the actual report bytes, for example from `sha256sum -- "$report"`. The `command` field is the exact metadata literal shown, not a receipt-generating command.

```json
{
  "version": 1,
  "status": "PASS",
  "branch": "<checked-out-feature-branch>",
  "head": "<actual-HEAD>",
  "tree": "<actual-HEAD-tree>",
  "base": "<actual-main-HEAD-merge-base>",
  "range": "main...HEAD",
  "outgoing_oid": "<same-actual-HEAD>",
  "clean_before": true,
  "clean_after": true,
  "command": "native review main...HEAD",
  "report": "<absolute-private-SSD-report-path>",
  "report_sha256": "<SHA256-of-that-actual-report>"
}
```

The actual report must have exactly one matching `HEAD`, `TREE`, `BASE`, and `RANGE` metadata row. Its first two and last two nonblank rows must be `VERDICT: PASS` and `ACTIONABLE_COUNT: 0`; every verdict/count row must agree. This schematic report is valid evidence only when replaced by the independent reviewer's real whole-range findings and actual Git identity:

```text
VERDICT: PASS
ACTIONABLE_COUNT: 0
HEAD: <actual-HEAD>
TREE: <actual-HEAD-tree>
BASE: <actual-main-HEAD-merge-base>
RANGE: main...HEAD
<independent whole-range review evidence>
VERDICT: PASS
ACTIONABLE_COUNT: 0
```

Replace the quoted path placeholders below with the real receipts. Push exactly one nondeletion update from the checked-out feature branch to the same remote branch name; this explicit refspec carries its current HEAD. Multiple updates, tags, deletes, protected branches, mismatched refs/OIDs, detached HEAD, dirty state, and stale or tampered receipts are rejected.

```bash
export HYMT_LOCAL_GATE_RECEIPT="<gate producer's actual printed receipt path>"
export HYMT_NATIVE_REVIEW_RECEIPT="<independent reviewer's actual receipt path>"
branch="$(git symbolic-ref --short HEAD)" &&
    git push origin "refs/heads/$branch:refs/heads/$branch"
```
