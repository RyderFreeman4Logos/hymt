---
name: hymt-info
description: "Use when viewing translated GNU info pages with hymt info, including original passthrough."
---

# Hymt Info

Use `hymt info` when a user wants a translated GNU info page.

## Commands

- Translate a topic: `hymt info coreutils`
- Translate a node path: `hymt info emacs buffers`
- Target language: `hymt info -l zh coreutils`
- Original passthrough: `hymt info --original coreutils`

## Behavior

- `--original` calls the system `info` command directly.
- Translated output is displayed through `$PAGER`, or `less -R` when `$PAGER` is unset and stdout is a TTY.
- If stdout is not a TTY, translated output is written directly to stdout for piping.
- Cache lookup order is user exec cache, shared exec cache, then live translation stored in the user cache.
- The underlying translation also uses the segment cache for repeated page content.
