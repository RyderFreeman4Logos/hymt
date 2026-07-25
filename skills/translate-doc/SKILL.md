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
- Each completed segment is written to the shared segment cache immediately, so interrupted runs resume cheaply.

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
- Completeness validation checks translated segments for minimum character ratio, paragraph retention, and Markdown heading preservation.
- Failed segments retry up to `[completeness].max_retries`, the shared completeness retry setting used by normal, streaming, batch, and `translate-doc` segment validation. After retries are exhausted, `hymt` still writes the best-effort output and emits `completeness_degraded_segments=…` on stderr. Top-level text/file/stdin translation exits non-zero so scripts detect degraded results; pass `--warn-only-completeness` or set `[completeness].warn_only = true` to keep exit 0 with warnings only. `translate-doc`, `batch`, and `exec` report the same stderr marker by default but do not fail the whole job for degraded segments.
- Source segments are bounded by the expansion/context budget and `[translation].max_source_tokens_per_segment` (default `384`, `0` disables). The conservative 7B-safe default prevents retries of a segment a model ends early; raise it only after validating complete output.

## Notes

- Use `skills/translate/SKILL.md` for general text translation, batch translation, recall, and config inspection.
- Use `translate-doc` for bilingual README maintenance and other Markdown-first automation.
