---
name: translate-doc
description: "Use when translating Markdown documents or README trees with hymt translate-doc and bilingual sync workflows."
---

# Translate Doc

Use `hymt translate-doc` when you want file-oriented Markdown translation instead of stdout-oriented ad hoc translation.

## Commands

- Default Simplified Chinese output: `hymt translate-doc README.md`
- Explicit target: `hymt translate-doc README.md -l ja`
- Explicit output path: `hymt translate-doc README.md -l zh --output README.zh-cn.md`
- Directory tree: `hymt translate-doc docs/ --recursive`
- Preserve a separate output tree: `hymt translate-doc docs/ --recursive --output-dir translated-docs`

## Behavior

- `translate-doc` only accepts Markdown sources.
- `--plan` is unsupported for `translate-doc` and rejects before config, backend, or output access. Use top-level `hymt --plan FILE` to inspect a file's translation plan.
- File output from text, stdin, file, `batch`, and `translate-doc` uses an exclusively created temporary in the destination directory, writes and flushes all bytes, closes it, then renames it into place. Failed or cancelled writes preserve the previous file and its hard-link aliases; successful replacement leaves old aliases unchanged. Valid leaf symlinks to regular files and directory symlinks remain links. Dangling links, nonregular targets, and permission errors are rejected. Existing ordinary permission bits are retained; new files follow the process umask. ACLs, extended attributes, ownership, special mode bits, power-loss durability, and concurrent path replacement are outside this guarantee. Validated cache/history written before an output error is not rolled back.
- For `-l zh`, output file names normalize to `.zh-cn.md`.
- Mixed-language Markdown uses the same paragraph-level partial translation rules as the main `hymt` command.
- Fenced code blocks are preserved.
- Progress is written to stderr as `[done/total] XX.XX% | elapsed ... | eta ... | NN.NN tok/s`.
- With `[cache].enabled = true` and without `--no-cache`, validated, non-degraded segments are admitted to the shared segment cache after whole-document validation. An interrupted run can reuse only entries already admitted.

## Config

```toml
[translation]
max_source_tokens_per_segment = 384

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 2
warn_only = true
```

- `translate-doc` uses non-streaming translation requests, buffers the results, and publishes each document only after validation.
- Completeness validation checks translated segments for output/input density, paragraph retention, Markdown structure, repeated URL/placeholder identities, JSON structure and values, and transport truncation. Budget-fitting ordinary inline Markdown paragraphs retain their complete request context and validate protected bytes/roles; references, math, escaped syntax, containers and oversized paragraphs keep source-owned splitting.
- Failed segments retry up to `[completeness].max_retries` across normal, streaming, batch, and `translate-doc` paths. By default, exhausted content or Markdown validation retains the complete original paragraph unchanged and continues translating other paragraphs. A visible notice outside Markdown containers identifies retained paragraphs by source byte and body line ranges; stderr reports `completeness_status=degraded_original_retained` and `completeness_degraded_segments=…`. Source ownership is fixed before reference expansion and long-paragraph splitting: failure of any piece restores its whole original paragraph once. Retained pieces are excluded from accepted segment and exec caches; degraded documents never populate offline replay proofs or cold timing samples. Streaming with retention enabled waits for validation and emits exactly the final marked output. Set `[completeness].warn_only = false` for strict failure, or use `--warn-only-completeness` to override strict configuration. Runtime, network, authentication, configuration, and cancellation errors remain errors. These mechanical checks do not prove semantic translation accuracy.
- Repeated URLs and placeholders retain every source occurrence. Atomic JSON keeps the same object keys, array lengths, value types, and non-string scalar values; only string values may translate. JSON validation exhaustion remains an error in every mode.
- Source segments are bounded by the expansion/context budget and `[translation].max_source_tokens_per_segment` (default `384`, `0` disables). The conservative 7B-safe default prevents retries of a segment a model ends early; raise it only after validating complete output.

## Notes

- Use `skills/translate/SKILL.md` for general text translation, batch translation, recall, and config inspection.
- Use `translate-doc` for bilingual README maintenance and other Markdown-first automation.
