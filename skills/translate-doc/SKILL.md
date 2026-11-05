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
max_retries = 2
warn_only = true
```

- `translate-doc` buffers results from the same SSE inference requests used by stdout and Telegram, then publishes each complete assembled document. Output delivery does not partition server cache entries.
- Admission checks response envelopes, transport termination, complete source coverage and ordered assembly. Completed model text, including empty strings, changed Markdown, omitted URLs/placeholders and non-JSON bodies, is accepted unchanged. Model wording and semantic omissions do not trigger retries. Source planning preserves protected bytes and request budgets.
- Failed segments retry up to `[completeness].max_retries` for buffered, batch, and `translate-doc` paths. By default, exhausted mechanical validation retains the complete original paragraph unchanged and continues translating other paragraphs. A visible notice outside Markdown containers identifies retained paragraphs by source byte and body line ranges; stderr reports `completeness_status=degraded_original_retained` and `completeness_degraded_segments=…`. Source ownership is fixed before reference expansion and long-paragraph splitting: failure of any piece restores its whole original paragraph once. Retained pieces are excluded from accepted segment and exec caches; degraded documents never populate offline replay proofs or cold timing samples. Explicit library Paragraph mode buffers each source owner for retention. Default Optimistic stdout fails when a chunk's initial progressive attempt becomes incomplete after nonempty content. If that attempt is incomplete without content, completeness retries remain fully buffered: unpublished drafts may recover under the configured retry budgets, and only a completed replacement is published once. Exhausted retries still fail in Optimistic mode. Set `[completeness].warn_only = false` for strict failure, or use `--warn-only-completeness` to override strict configuration. Runtime, network, authentication, configuration, and cancellation errors remain errors. These mechanical checks do not prove semantic translation accuracy.
- Source splitting retains every URL/placeholder occurrence and preserves atomic JSON input as one request within its budget. Model output is not checked against source identities or JSON schema.
- Source segments are bounded by the expansion/context budget and `[translation].max_source_tokens_per_segment` (default `384`, `0` disables). The conservative 7B-safe default prevents retries of a segment a model ends early; raise it only after validating complete output.

## Notes

- Use `skills/translate/SKILL.md` for general text translation, batch translation, recall, and config inspection.
- Use `translate-doc` for bilingual README maintenance and other Markdown-first automation.
