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

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 2
```

- `stream` controls streaming requests to the endpoint.
- Completeness validation checks translated segments for minimum character ratio, paragraph retention, and Markdown heading preservation.
- Failed segments retry up to `[completeness].max_retries`; after retries are exhausted, `hymt` warns and continues with the best attempt.

## Notes

- Use `skills/translate/SKILL.md` for general text translation, batch translation, recall, and config inspection.
- Use `translate-doc` for bilingual README maintenance and other Markdown-first automation.
