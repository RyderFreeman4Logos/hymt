---
name: hymt-man
description: "Use when viewing translated Unix manpages with hymt man, including apropos searches, specific sections, cache refreshes, and original passthrough."
---

# Hymt Man

Use `hymt man` when a user wants a translated manpage or apropos result.

## Commands

- Translate a page: `hymt man git-rebase`
- Specific section: `hymt man 5 crontab`
- Apropos search: `hymt man -k "file system"`
- Target language: `hymt man -t zh git-rebase`
- Original passthrough: `hymt man --original git-rebase`
- Force re-translation: `hymt man --refresh git-rebase`

## Behavior

- `--original` calls the system `man` command directly.
- Translated output is displayed through `$PAGER`, or `less -R` when `$PAGER` is unset and stdout is a TTY.
- If stdout is not a TTY, translated output is written directly to stdout for piping.
- Cache lookup order is user exec cache, shared exec cache, then live translation stored in the user cache.
- The underlying translation also uses the segment cache, so repeated manpage sections can reuse prior segment translations.
