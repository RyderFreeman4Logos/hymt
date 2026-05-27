---
name: translate-doc
description: "Use when translating Markdown documents or README trees with hymt translate-doc, including watch-mode retries and bilingual sync workflows."
---

# Translate Doc

Use `hymt translate-doc` when you want file-oriented Markdown translation instead of stdout-oriented ad hoc translation.

## Commands

- Default Simplified Chinese output: `hymt translate-doc README.md`
- Explicit target: `hymt translate-doc README.md -l ja`
- Explicit output path: `hymt translate-doc README.md -l zh --output README.zh-cn.md`
- Directory tree: `hymt translate-doc docs/ --recursive`
- Preserve a separate output tree: `hymt translate-doc docs/ --recursive --output-dir translated-docs`
- Watch a file and re-translate on change: `hymt translate-doc README.md --watch`

## Behavior

- `translate-doc` only accepts Markdown sources.
- For `-l zh`, output file names normalize to `.zh-cn.md`.
- Mixed-language Markdown uses the same paragraph-level partial translation rules as the main `hymt` command.
- Fenced code blocks are preserved.
- Progress is written to stderr as `[done/total] XX.XX% | elapsed ... | eta ... | NN.NN tok/s`.
- Each completed segment is written to the shared segment cache immediately, so interrupted runs resume cheaply.
- In `--watch` mode, source changes cancel the in-flight translation attempt, then `hymt` re-segments and retries with cache reuse.
- `watchfiles` is used when installed; otherwise `translate-doc` falls back to polling.

## Config

```toml
[translation]
stream = true
max_retranslation_retries = 10
```

- `stream` controls streaming requests to the endpoint.
- `max_retranslation_retries` bounds how many source-change retries are allowed in one watch cycle.

## Notes

- Use `skills/translate/SKILL.md` for general text translation, batch translation, recall, and config inspection.
- Use `translate-doc` for bilingual README maintenance and other Markdown-first automation.
