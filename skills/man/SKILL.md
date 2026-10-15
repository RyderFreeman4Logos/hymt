---
name: hymt-man
description: "Use when viewing translated Unix manpages with hymt man, including apropos searches, specific sections, and original passthrough."
---

# Hymt Man

Use `hymt man` when a user wants a translated manpage or apropos result.

## Commands

- Translate a page: `hymt man git-rebase`
- Specific section: `hymt man 5 crontab`
- Apropos search: `hymt man -k "file system"`
- Target language: `hymt man -l zh git-rebase`
- Original passthrough: `hymt man --original git-rebase`

## Behavior

- `--original` calls the system `man` command directly.
- Translated output is displayed through `$PAGER`, or `less -R` when `$PAGER` is unset and stdout is a TTY.
- If stdout is not a TTY, translated output is written directly to stdout for piping.
- With `[cache].enabled = true` and without `--no-cache`, lookup checks the user exec cache, then the shared exec cache; eligible live translations enter the user cache after validation.
- Under the same opt-in, validated, non-degraded segments can reuse the segment cache. Cache access is off by default; history and timing remain available.
