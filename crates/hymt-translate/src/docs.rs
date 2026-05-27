//! Man and info page translation via ExecCache.
//!
//! Captures man/info output, strips overstrike formatting, translates via the
//! exec cache, and pages the result using the user's `$PAGER`.

use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};

use anyhow::Result;

use hymt_cache::history::HistoryDB;
use hymt_cache::ExecCache;
use hymt_client::TranslationClient;
use hymt_core::config::HotConfig;
use hymt_core::language::resolve_target_language;
use hymt_segment::Segmenter;

use crate::exec_wrapper::translate_cached;
use crate::translate::TranslationCtx;

// ── Public types ─────────────────────────────────────────────────────────────

/// Shared options for man/info translation commands.
pub struct ManInfoOpts<'a> {
    pub target_lang: &'a str,
    pub config: &'a HotConfig,
    pub client: &'a TranslationClient,
    pub segmenter: &'a Segmenter,
    pub history: &'a HistoryDB,
    /// Show original (untranslated) output.
    pub original: bool,
    /// Whether the caller explicitly specified the target language.
    pub explicit_target: bool,
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Capture, translate, and page a man page.
///
/// Returns the pager exit code, or 0 when stdout is not a TTY (direct write).
pub async fn run_man_command(args: &[&str], opts: &ManInfoOpts<'_>) -> Result<i32> {
    if args.is_empty() {
        anyhow::bail!("man page or man arguments are required");
    }
    if opts.original {
        let code = Command::new("man")
            .args(args)
            .status()
            .map(|s| s.code().unwrap_or(1))
            .unwrap_or(1);
        return Ok(code);
    }
    let text = capture_man(args)?;
    let effective_lang =
        resolve_effective_lang(&text, opts.target_lang, opts.config, opts.explicit_target);
    let subcmd = args.join(" ");
    let cache = ExecCache::new(opts.config.exec_shared_cache_path());
    let tctx = TranslationCtx {
        config: opts.config,
        client: opts.client,
        segmenter: opts.segmenter,
        history: opts.history,
    };
    let translated =
        translate_cached("man", &subcmd, &text, &effective_lang, &cache, &tctx).await?;
    page_text(&translated)
}

/// Capture, translate, and page an info page.
pub async fn run_info_command(args: &[&str], opts: &ManInfoOpts<'_>) -> Result<i32> {
    if args.is_empty() {
        anyhow::bail!("info topic or info arguments are required");
    }
    if opts.original {
        let code = Command::new("info")
            .args(args)
            .status()
            .map(|s| s.code().unwrap_or(1))
            .unwrap_or(1);
        return Ok(code);
    }
    let text = capture_info(args)?;
    let effective_lang =
        resolve_effective_lang(&text, opts.target_lang, opts.config, opts.explicit_target);
    let subcmd = args.join(" ");
    let cache = ExecCache::new(opts.config.exec_shared_cache_path());
    let tctx = TranslationCtx {
        config: opts.config,
        client: opts.client,
        segmenter: opts.segmenter,
        history: opts.history,
    };
    let translated =
        translate_cached("info", &subcmd, &text, &effective_lang, &cache, &tctx).await?;
    page_text(&translated)
}

// ── Capture helpers (pub(crate) for reuse in precache) ───────────────────────

/// Run `man <args>` with `MANPAGER=cat` and strip overstrike formatting.
pub(crate) fn capture_man(args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("man");
    cmd.args(args)
        .env("MANPAGER", "cat")
        .env("PAGER", "cat")
        .env_remove("LESS");
    if std::env::var("MANWIDTH").is_err() {
        cmd.env("MANWIDTH", "100");
    }
    let output = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run man: {e}"))?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            format!("man exited with {:?}", output.status.code())
        } else {
            msg
        };
        anyhow::bail!("{msg}");
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(strip_overstrikes(&text))
}

fn capture_info(args: &[&str]) -> Result<String> {
    let output = Command::new("info")
        .arg("--output=-")
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run info: {e}"))?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            format!("info exited with {:?}", output.status.code())
        } else {
            msg
        };
        anyhow::bail!("{msg}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ── Overstrike stripping ──────────────────────────────────────────────────────

/// Remove man page overstrike formatting (`char BS char` → `char`, `_ BS char` → `char`).
///
/// Iterates until no more backspace sequences remain (handles multiply-struck chars).
pub fn strip_overstrikes(text: &str) -> String {
    let mut result = text.to_owned();
    loop {
        let before_len = result.len();
        let chars: Vec<char> = result.chars().collect();
        let mut out = String::with_capacity(result.len());
        let mut i = 0;
        while i < chars.len() {
            if i + 1 < chars.len() && chars[i + 1] == '\x08' {
                // Skip char[i] + backspace — the visible character follows
                i += 2;
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        result = out;
        if result.len() == before_len {
            break;
        }
    }
    result
}

// ── Language resolution ───────────────────────────────────────────────────────

fn resolve_effective_lang(
    text: &str,
    target_lang: &str,
    config: &HotConfig,
    explicit: bool,
) -> String {
    if explicit {
        return target_lang.to_owned();
    }
    // Walk backward from the byte limit to the nearest char boundary.
    let max = text.len().min(4096);
    let end = (0..=max)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    let sample = &text[..end];
    resolve_target_language(
        sample,
        target_lang,
        &config.primary_lang(),
        &config.secondary_lang(),
        false,
    )
}

// ── Pager ─────────────────────────────────────────────────────────────────────

fn page_text(text: &str) -> Result<i32> {
    let output = if text.ends_with('\n') {
        text.to_owned()
    } else {
        format!("{text}\n")
    };

    if !std::io::stdout().is_terminal() {
        print!("{output}");
        return Ok(0);
    }

    let pager_env = std::env::var("PAGER").unwrap_or_else(|_| "less -R".to_owned());
    let parts: Vec<&str> = pager_env.split_whitespace().collect();
    if parts.is_empty() {
        print!("{output}");
        return Ok(0);
    }

    let mut child = match Command::new(parts[0])
        .args(&parts[1..])
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            print!("{output}");
            return Ok(0);
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(output.as_bytes());
    }
    Ok(child.wait().map(|s| s.code().unwrap_or(0)).unwrap_or(0))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_plain_text_unchanged() {
        assert_eq!(strip_overstrikes("Hello, world!"), "Hello, world!");
    }

    #[test]
    fn strip_empty_unchanged() {
        assert_eq!(strip_overstrikes(""), "");
    }

    #[test]
    fn strip_bold_formatting() {
        // Man page bold: each char is char BS char → char
        // "bold" as bold: b\x08b o\x08o l\x08l d\x08d
        let input = "b\x08bo\x08ol\x08ld\x08d";
        assert_eq!(strip_overstrikes(input), "bold");
    }

    #[test]
    fn strip_underline_formatting() {
        // Man page underline: _ BS char → char
        // "word" underlined: _\x08w _\x08o _\x08r _\x08d
        let input = "_\x08w_\x08o_\x08r_\x08d";
        assert_eq!(strip_overstrikes(input), "word");
    }

    #[test]
    fn strip_multiple_overstrikes_converges() {
        // Triple-struck: a BS a BS a → a (two passes needed)
        let input = "a\x08a\x08a";
        assert_eq!(strip_overstrikes(input), "a");
    }

    #[test]
    fn strip_mixed_text() {
        // "foo BAR baz" where BAR is bold
        let input = "foo B\x08BA\x08AR\x08R baz";
        assert_eq!(strip_overstrikes(input), "foo BAR baz");
    }
}
