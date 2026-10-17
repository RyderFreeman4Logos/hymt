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
- Global `--plan` supports text, stdin, file input, and `batch`. Other subcommands reject it before reading config, contacting the backend, or changing state. Supported planners may read backend metadata and compatible cache/history data but do not translate or publish outputs.
- Without an explicit `-l/--lang`, hymt routes between `[language].primary` (default `zh`) and `[language].secondary` (default `en`): mostly-primary input targets secondary, otherwise it targets primary.
- Mixed-language documents targeting `zh`, `zh-Hant`, or `yue` preserve only high-confidence CJK target-language paragraphs by default; code blocks and frontmatter are always preserved. Use `--force-translate-all` (or `[translation].force_translate_all = true`) to submit every non-code paragraph, or `--no-language-detection` / `[translation].language_detection = false` to disable detection for one run or configuration.

## Batch translate directories

- Preview a directory without writing files: `hymt batch ./docs -l zh --dry-run`
- Include subdirectories in the preview: `hymt batch ./docs -l zh --dry-run --recursive`
- Translate and write outputs: `hymt batch ./docs -l zh`
- Skip confirmation for automation: `hymt batch ./docs -l zh --yes`
- Write to a separate tree while preserving source-relative paths: `hymt batch ./docs -l zh --output-dir translated-docs`
- Batch mode scans the top-level directory by default; add `--recursive` to descend into subdirectories. An explicit root directory symlink is accepted; interior symlinks are not followed or selected. It selects `.txt` and `.md` files.
- Output names are `{stem}.{target}.{ext}` using the effective target; with default routing, English files write `README.zh-cn.md` and mostly-primary files write `README.en.md`.
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
- `hymt backend inspect` — query the selected service without printing credentials; compare configured and resolved backend state
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
- File output from text, stdin, file, `batch`, and `translate-doc` uses an exclusively created temporary in the destination directory, writes and flushes all bytes, closes it, then renames it into place. Failed or cancelled writes preserve the previous file and its hard-link aliases; successful replacement leaves old aliases unchanged. Valid leaf symlinks to regular files and directory symlinks remain links. Dangling links, nonregular targets, and permission errors are rejected. Existing ordinary permission bits are retained; new files follow the process umask. ACLs, extended attributes, ownership, special mode bits, power-loss durability, and concurrent path replacement are outside this guarantee. Validated cache/history written before an output error is not rolled back.
- With piped stdin, a single positional `.` is treated as a stdin placeholder so commands like `producer | hymt .` translate the pipe instead of the literal dot.
- Explicit `-l/--lang` disables default language routing and uses the requested target for prompts, cache/history keys, and output suffixes.
- Target-language paragraph preservation is CJK-only (`zh`, `zh-Hant`, `yue`): explicit `-l/--lang` selects the target but does not disable it. High-confidence target paragraphs are reconstructed byte-for-byte; non-Chinese targets translate every non-code paragraph. `--plan` reports each paragraph's detection metadata and translation decision.
- Fenced code blocks and leading YAML frontmatter are excluded from language detection and translation in every mode.
- Markdown link/image labels remain translatable; shortcut/collapsed references reconstruct as explicit references bound to the original parser-resolved ID. Reference targets/titles, inline syntax and backslash-escaped punctuation remain source-owned.
- Link-like text that CommonMark parses as literal is malformed source, not a link to repair. If a translation turns it into a parsed link, reject that candidate before output or cache/history writes and retry within the completeness budget; after exhaustion, the default retains the marked complete original paragraph, while `[completeness].warn_only = false` returns an error. Strict-mode Optimistic requests containing unparsed inline-link delimiters or unresolved reference candidates are validated before publication.
- Stdout defaults to `--stream`. With the default `[completeness].warn_only = true`, streaming uses Validated publication: all output waits for document validation, then replays ordered final chunks, including any marked original paragraphs. With explicit `warn_only = false`, normal text/stdin may use Optimistic mode, emitting cached prefixes and segment-0 tokens before whole-document validation; input containing unparsed inline-link delimiters or unresolved reference candidates and CLI help/usage/options-like input use Validated publication. Uncached segment 0 starts first; its first non-empty backend token releases remaining requests. `--output <path>`, `--no-stream`, and `--no-streaming` default to buffered output.
- `--concurrency N` overrides the client semaphore. `--concurrency 1` serializes provider work. When strict mode selects Optimistic, it flushes contiguous completed segments in document order; Validated mode waits for whole-document validation before replay.
- `--debug-chunk-timing` (also `translation.debug_chunk_timing` or `HYMT_DEBUG_CHUNK_TIMING=1`) prints per-segment `queue_enter` / `request_start` / `first_token` / `complete` / completeness-retry markers on stderr only.
- Fresh streaming cache entries wait for document validation and bounded delivery. An open receiver and reserved terminal slot admit cache/history writes before `AllDone`. Closing after `AllDone` does not cancel success; store failures remain warnings, without rollback.
- Progress is written to stderr as `[done/total] XX.XX% | elapsed 2m47s | eta 1m23s | NN.NN tok/s`.
- Identical source segments, target language, template type, and template-specific options reuse cached segment translations only with the same profile and verified inference fingerprint while segment caching is enabled. The fingerprint includes configured endpoint/model/backend, profile model/tokenizer sources, explicit sampling and observed runtime metadata. It cannot attest to an invisible in-place model-byte replacement; `hymt history --clear` removes segment entries and task history before reuse.
- With `[cache].enabled = true`, fully validated matching task proofs can replay locally before live preflight, including strict preflight, while the service is unreachable. Proofs bind current rendered prompts, adapter request bytes, source/target, tokenizer, ordered plan, limits and policy; runtime evidence remains historical and isolated. Only Verified, complete initial attempts populate proofs; degraded/retried results and legacy segment hits do not. Changed/corrupt/legacy/empty proofs and explicit bypass use normal live behavior. Replays never calibrate cold timing. Streaming and buffered contracts match separately; `hymt history --clear` deletes proofs with tasks.
- Application segment and whole-output caches default to off; this leaves service inference caching active. Explicit `--no-cache` (alias `--disable-cache`) bypasses both application and service cache reads and writes. Automatic validation retries bypass service caching on their own client clone without changing sibling requests. The optional shared plaintext inference cache is a separate service: `just install-inference-cache` installs its script and user unit at stable paths and restarts it if active. The supplied unit listens at `100.78.159.38:8402`, forwards to `100.78.159.38:8401`, and shares complete raw responses across API/VK credentials. Restrict direct access to trusted Tailnet callers or place it behind the existing authenticating gateway; credentials do not partition hits. Clients opt in with `[endpoint].url = "http://100.78.159.38:8402/v1"`, `backend = "llama_cpp"`, and `[endpoint].model` set to an actual upstream `/v1/models` ID or alias; the example config's empty model cannot produce cache hits. After replacing model bytes without changing observed upstream metadata, run `python3 "$HOME/.local/share/hymt/inference-cache.py" clear --database "$HOME/.local/state/hymt/inference-cache.sqlite3"`; this also invalidates in-flight admission. The service probes upstream identity first. If its HTTP endpoint remains reachable during a classified model-backend outage, exact populated JSON/SSE responses can replay using retained same-origin active identity/epoch and historical planning evidence, including one bounded exact false-EOS tokenization result. The CLI labels this planning historical; strict preflight still requires live verification. Misses and explicit bypass need the backend. Authentication rejection, malformed metadata, TLS verification errors, and local failures do not permit replay. Origin/model changes and clear invalidate old planning. Each database has a private one-byte `.guard` file and permits one cache process. Interrupted identity probes or failed invalidation block historical replay until complete online recovery; keep the guard with its database. Explicit bypass remains transparent and does not observe cache identity. This is separate from translation validation, history, and segment caching; it does not cover an unreachable cache service.
- Segments validate density, paragraph coverage, Markdown structure, repeated URLs/placeholders, JSON identities and transport truncation. Atomic JSON cannot be split and remains strict. Failed segments retry up to `[completeness].max_retries` across normal, streaming, batch, and `translate-doc` paths. By default, exhausted content or Markdown validation retains the complete original paragraph unchanged and continues translating other paragraphs. A visible notice outside Markdown containers identifies retained paragraphs by source byte and body line ranges; stderr reports `completeness_status=degraded_original_retained` and `completeness_degraded_segments=…`. Source ownership is fixed before reference expansion and long-paragraph splitting: failure of any piece restores its whole original paragraph once. Retained pieces are excluded from accepted segment and exec caches; degraded documents never populate offline replay proofs or cold timing samples. Streaming with retention enabled waits for validation and emits exactly the final marked output. Set `[completeness].warn_only = false` for strict failure, or use `--warn-only-completeness` to override strict configuration. Runtime, network, authentication, configuration, and cancellation errors remain errors. These mechanical checks do not prove semantic translation accuracy.
- Markdown failures use the same retry budget and complete-original fallback. Accepted output is reconstructed and validated before publication; failed candidates are never emitted in retention mode.
- Repeated URLs and placeholders retain every source occurrence. Atomic JSON keeps the same object keys, array lengths, value types, and non-string scalar values; only string values may translate. JSON validation exhaustion remains an error in every mode.
- Source segments are bounded by the final request/context budget and by `[translation].max_source_tokens_per_segment` (default `384`, `0` disables). This conservative 7B-safe default prevents repeated retries of a segment that a deployed model ends early; raise it only after validating complete output. Profiled local-tokenizer plans include chat framing, prompt/context, assistant marker, completeness-retry reservation, and output reservation. Fenced code is preserved as source. When the planner excludes an oversized protected Markdown block, including a table, it warns and preserves the source bytes instead of splitting them for the model; surrounding prose can still translate. Atomic JSON that exceeds its budget is rejected before any translation request.
- `translate-doc` buffers results; fresh segment-cache writes wait for whole-document structural validation. Existing valid entries remain reusable, but interruption before validation saves no new segments. Degraded results are not freshly cached.
- Timing forecasts freeze before requests and require three cold observations with matching inference identity, prompt/config version and token counting units. They scale with pending model-visible source tokens. Each translation invocation uses one detached config snapshot for planning, requests, and history. Application-cache hits, shared-cache `HIT`/unknown provenance, legacy rows, transport retries, and completeness retries do not calibrate or trigger cold-generation divergence warnings. Explicit shared-cache `MISS`/`BYPASS` responses qualify only on the first transport attempt; ordinary history and recall remain available. Calibrated runs retain `[timing].divergence_threshold` (default `2.0`) warnings.
- Config lives at `~/.config/hymt/config.toml`; `[language].primary` and `[language].secondary` configure default routing, `[translation].max_source_tokens_per_segment` caps per-segment source size, and `[completeness]` configures segment validation thresholds plus `warn_only`. `[telegram]` configures the optional bot (disabled until `enabled = true`).
- The tokenizer is cached at `~/.cache/hymt/tokenizer/tokenizer.json`.
- Translation commands use the cached tokenizer with a known profile chat template when present; otherwise they visibly warn and use a conservative `2x` approximate input budget plus `64` framing tokens. Run `hymt tokenizer download` to fetch the tokenizer explicitly.
- Rendered translation prompts are versioned as `hy-mt2-prompts/v2`. This ID is recorded in inference fingerprints, task history, `--plan` token-budget diagnostics, and any benchmark metadata; increment it whenever model-visible prompt text changes so old cache entries and benchmark runs remain distinguishable.
- Set `[translation].strict_token_budget = true` to reject approximate planning (including generic/unprofiled endpoints or builds without the tokenizer feature).
