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
- For `-l zh`, output file names normalize to `.zh-cn.md`.
- Mixed-language Markdown uses the same paragraph-level partial translation rules as the main `hymt` command.
- Fenced code blocks are preserved.
- Progress is written to stderr as `[done/total] XX.XX% | elapsed ... | eta ... | NN.NN tok/s`.
- With `[cache].enabled = true` and without `--no-cache`, validated, non-degraded segments are admitted to the shared segment cache after whole-document validation. An interrupted run can reuse only entries already admitted.

## Config

```toml
[translation]
stream = true
max_source_tokens_per_segment = 384

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 2
warn_only = false
```

- `stream` controls streaming requests to the endpoint.
- Completeness validation checks translated segments for output/input density, paragraph retention, Markdown structure, repeated URL/placeholder identities, JSON structure and values, and transport truncation.
- Failed segments retry up to `[completeness].max_retries`, the shared completeness retry setting used by normal, streaming, batch, and `translate-doc` segment validation. After retries are exhausted, the default is an error before document output, segment-cache storage, or successful history publication. Pass `--warn-only-completeness` or set `[completeness].warn_only = true` to explicitly allow a non-empty best attempt that meets the 33% approximate source-token floor; it emits `completeness_degraded_segments=…` on stderr. Empty or below-floor results remain errors, and structural validation still applies. A rejected `translate-doc` document leaves any existing output file untouched; directory translation continues with later documents, then exits non-zero if any document failed. `batch` also fails before writing the rejected result. `exec` reports translation errors without changing the wrapped command's exit status. Validated streaming withholds failed output; optimistic streaming cannot retract emitted bytes, but a failed attempt does not become document success.
- Repeated URLs and placeholders retain every source occurrence. Atomic JSON keeps the same object keys, array lengths, value types, and non-string scalar values; only string values may translate. Identity or Markdown ownership failures remain errors even in warn-only mode. These mechanical checks do not prove semantic accuracy.
- Source segments are bounded by the expansion/context budget and `[translation].max_source_tokens_per_segment` (default `384`, `0` disables). The conservative 7B-safe default prevents retries of a segment a model ends early; raise it only after validating complete output.

## Notes

- Use `skills/translate/SKILL.md` for general text translation, batch translation, recall, and config inspection.
- Use `translate-doc` for bilingual README maintenance and other Markdown-first automation.
