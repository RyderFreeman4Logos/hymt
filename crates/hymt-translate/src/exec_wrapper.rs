//! Run a subprocess, capture its output, translate it, and print the translation.
//!
//! The command is run synchronously; the translation is async.  Binary output,
//! structured data (JSON, XML, YAML), and build-progress lines are skipped.

use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;

use anyhow::Result;

use hymt_cache::history::HistoryDB;
use hymt_cache::ExecCache;
use hymt_client::TranslationClient;
use hymt_core::config::HotConfig;
use hymt_core::language::resolve_target_language;
use hymt_core::templates::{PromptOpts, TemplateType};
use hymt_segment::Segmenter;

use crate::translate::{translate_text, TranslationCtx};

const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_RESET: &str = "\x1b[0m";

// ── Public entry point ────────────────────────────────────────────────────────

/// Run `command`, capture stdout/stderr, translate the output, and print it.
/// Returns the subprocess exit code.
pub async fn run_exec_command(
    command: &[&str],
    target_lang: &str,
    config: &HotConfig,
    client: &TranslationClient,
    segmenter: &Segmenter,
    history: &HistoryDB,
    explicit_target: bool,
) -> Result<i32> {
    if command.is_empty() {
        anyhow::bail!("command is required");
    }

    let (stdout_bytes, stderr_bytes, exit_code) = run_subprocess(command)?;

    let effective_lang = if explicit_target {
        target_lang.to_owned()
    } else {
        // Auto-detect from stdout sample
        let sample = std::str::from_utf8(&stdout_bytes[..stdout_bytes.len().min(4096)])
            .unwrap_or("")
            .to_owned();
        resolve_target_language(
            &sample,
            target_lang,
            &config.primary_lang(),
            &config.secondary_lang(),
            false,
        )
    };

    let exe = Path::new(command[0])
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| command[0].to_owned());
    let subcmd = command.get(1).copied().unwrap_or("").to_owned();

    let tctx = TranslationCtx {
        config,
        client,
        segmenter,
        history,
    };

    // Translate stderr if configured
    if config.exec_translate_stderr() && !stderr_bytes.is_empty() {
        if let Some(text) = decode_for_translation(&stderr_bytes) {
            let cache = ExecCache::new(config.exec_shared_cache_path());
            match translate_cached(&exe, &subcmd, &text, &effective_lang, &cache, &tctx).await {
                Ok(translated) => {
                    write_translation("stderr", &translated, false, ANSI_YELLOW);
                }
                Err(e) => eprintln!("hymt: stderr translation failed: {e}"),
            }
        }
    }

    // Translate stdout if not binary/structured/progress
    let should_translate = should_translate_stdout(command, &stdout_bytes, config);
    if should_translate {
        if let Some(text) = decode_for_translation(&stdout_bytes) {
            let cache = ExecCache::new(config.exec_shared_cache_path());
            let use_tty = std::io::stdout().is_terminal();
            match translate_cached(&exe, &subcmd, &text, &effective_lang, &cache, &tctx).await {
                Ok(translated) => {
                    write_translation("stdout", &translated, use_tty, ANSI_CYAN);
                }
                Err(e) => eprintln!("hymt: stdout translation failed: {e}"),
            }
        }
    }

    Ok(exit_code)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn run_subprocess(command: &[&str]) -> Result<(Vec<u8>, Vec<u8>, i32)> {
    let output = Command::new(command[0])
        .args(&command[1..])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run {:?}: {e}", command[0]))?;

    // Show output to the user in real-time is not possible with Command::output(),
    // so we print it after capture.
    use std::io::Write;
    std::io::stdout().write_all(&output.stdout).ok();
    std::io::stderr().write_all(&output.stderr).ok();

    let code = output.status.code().unwrap_or(1);
    Ok((output.stdout, output.stderr, code))
}

pub(crate) async fn translate_cached(
    command: &str,
    subcommand: &str,
    text: &str,
    target_lang: &str,
    cache: &ExecCache,
    ctx: &TranslationCtx<'_>,
) -> Result<String> {
    if let Ok(Some(cached)) = cache.find(command, subcommand, text, target_lang) {
        return Ok(cached);
    }

    let opts = PromptOpts::default();
    let translated = translate_text(text, target_lang, &TemplateType::Default, &opts, ctx).await?;

    if let Err(e) = cache.store_user(command, subcommand, text, target_lang, &translated) {
        eprintln!("Warning: exec cache store failed: {e}");
    }
    Ok(translated)
}

fn write_translation(stream_name: &str, translated: &str, use_color: bool, color: &str) {
    let prefix = format!("\n[hymt translated {stream_name}]\n");
    if use_color {
        eprint!("{color}{prefix}{translated}{ANSI_RESET}");
    } else {
        eprint!("{prefix}{translated}");
    }
    if !translated.ends_with('\n') {
        eprintln!();
    }
}

fn should_translate_stdout(command: &[&str], output: &[u8], config: &HotConfig) -> bool {
    if output.is_empty() {
        return false;
    }
    if !config.exec_translate_stdout() {
        return false;
    }
    let exe = Path::new(command[0])
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if config.exec_skip_commands().contains(&exe) {
        return false;
    }
    if matches_skip_pattern(command, &config.exec_skip_patterns()) {
        return false;
    }
    if looks_binary(output) {
        return false;
    }
    let text = match decode_for_translation(output) {
        Some(t) => t,
        None => return false,
    };
    if looks_structured(&text) || looks_like_build_progress(&text) {
        return false;
    }
    true
}

fn matches_skip_pattern(command: &[&str], patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let exe = Path::new(command[0])
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| command[0].to_owned());
    let full = command.join(" ");
    for pattern in patterns {
        for candidate in &[&exe, command[0], &full] {
            if glob_match(candidate, pattern) {
                return true;
            }
        }
    }
    false
}

fn glob_match(text: &str, pattern: &str) -> bool {
    // Simple glob: only * wildcard supported
    let mut t = text;
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return t == pattern;
    }
    if !t.starts_with(parts[0]) {
        return false;
    }
    t = &t[parts[0].len()..];
    for (i, part) in parts[1..].iter().enumerate() {
        if i == parts.len() - 2 {
            // Last part must match end
            return t.ends_with(part);
        }
        if let Some(pos) = t.find(part) {
            t = &t[pos + part.len()..];
        } else {
            return false;
        }
    }
    true
}

pub(crate) fn decode_for_translation(output: &[u8]) -> Option<String> {
    if looks_binary(output) {
        return None;
    }
    Some(String::from_utf8_lossy(output).into_owned())
}

// ── Detection functions (pub for tests) ───────────────────────────────────────

/// Returns `true` if `output` appears to be binary data.
///
/// Heuristic: null bytes OR >5% of the first 4 KiB are non-whitespace ASCII
/// control characters.
pub fn looks_binary(output: &[u8]) -> bool {
    let sample = &output[..output.len().min(4096)];
    if sample.contains(&0u8) {
        return true;
    }
    if sample.is_empty() {
        return false;
    }
    const ALLOWED: &[u8] = &[7, 8, 9, 10, 12, 13, 27]; // bell, bs, tab, lf, ff, cr, esc
    let control_count = sample
        .iter()
        .filter(|&&b| b < 32 && !ALLOWED.contains(&b))
        .count();
    control_count * 100 / sample.len() > 5
}

/// Returns `true` if `text` looks like structured data (JSON, XML, or YAML).
pub fn looks_structured(text: &str) -> bool {
    let stripped = text.trim();
    if stripped.is_empty() {
        return false;
    }
    looks_json(stripped) || looks_xml(stripped) || looks_yaml(stripped)
}

fn looks_json(text: &str) -> bool {
    if !matches!(text.chars().next(), Some('{') | Some('[')) {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(text).is_ok()
}

fn looks_xml(text: &str) -> bool {
    if !text.starts_with('<') {
        return false;
    }
    let first_line = text.lines().next().unwrap_or("");
    first_line.contains('>')
}

fn looks_yaml(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .collect();
    if lines.is_empty() {
        return false;
    }
    if lines[0].trim() == "---" || lines[0].trim() == "..." {
        return true;
    }
    let kv = lines[..lines.len().min(20)]
        .iter()
        .filter(|l| l.split('#').next().unwrap_or("").contains(':'))
        .count();
    let sample_size = lines.len().min(20);
    sample_size >= 3 && kv * 10 >= sample_size * 8 // ≥80%
}

/// Returns `true` if every non-empty line starts with a build-progress verb.
pub fn looks_like_build_progress(text: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "compiling ",
        "checking ",
        "building ",
        "running ",
        "finished ",
        "linking ",
        "generating ",
    ];
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return false;
    }
    // If any line has an error/warning keyword, not build progress
    if lines
        .iter()
        .any(|l| l.contains("error") || l.contains("warning"))
    {
        return false;
    }
    lines
        .iter()
        .all(|l| PREFIXES.iter().any(|p| l.to_lowercase().starts_with(p)))
}

// ── Agent-descendant detection (Linux-specific) ───────────────────────────────

/// Returns `true` if the process tree contains a known AI-agent process.
#[cfg(target_os = "linux")]
pub fn is_agent_descendant() -> bool {
    const AGENTS: &[&str] = &[
        "claude",
        "claude-code",
        "csa",
        "codex",
        "gemini-cli",
        "gemini",
        "opencode",
        "aider",
        "cursor",
        "copilot",
    ];
    let mut pid = std::process::id();
    loop {
        if pid <= 1 {
            return false;
        }
        let comm_path = format!("/proc/{pid}/comm");
        if let Ok(comm) = std::fs::read_to_string(&comm_path) {
            let comm = comm.trim().to_lowercase();
            if AGENTS.iter().any(|&a| comm == a) {
                return true;
            }
        }
        // Read parent PID from /proc/{pid}/status
        let status_path = format!("/proc/{pid}/status");
        let status = match std::fs::read_to_string(&status_path) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let ppid = status
            .lines()
            .find(|l| l.starts_with("PPid:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u32>().ok());
        match ppid {
            Some(p) if p > 0 => pid = p,
            _ => return false,
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn is_agent_descendant() -> bool {
    false
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── looks_binary ─────────────────────────────────────────────────────────

    #[test]
    fn binary_null_byte_is_binary() {
        assert!(looks_binary(b"hello\x00world"));
    }

    #[test]
    fn binary_empty_is_not_binary() {
        assert!(!looks_binary(b""));
    }

    #[test]
    fn binary_plain_text_is_not_binary() {
        assert!(!looks_binary(b"Hello, world!\nThis is normal text.\n"));
    }

    #[test]
    fn binary_high_control_ratio_is_binary() {
        // 10 control bytes among 20 bytes = 50% > 5%
        let data: Vec<u8> = (0u8..20).collect();
        assert!(looks_binary(&data));
    }

    #[test]
    fn binary_ansi_escape_is_not_binary() {
        // ESC (27) is in the allowed set
        let data = b"\x1b[0mHello\x1b[1mWorld\x1b[0m";
        assert!(!looks_binary(data));
    }

    // ── looks_structured ─────────────────────────────────────────────────────

    #[test]
    fn structured_valid_json_object() {
        assert!(looks_structured(r#"{"key": "value"}"#));
    }

    #[test]
    fn structured_valid_json_array() {
        assert!(looks_structured(r#"[1, 2, 3]"#));
    }

    #[test]
    fn structured_invalid_json_not_structured() {
        assert!(!looks_structured("{not valid json}"));
    }

    #[test]
    fn structured_xml_first_line_has_tag() {
        assert!(looks_structured("<root>\n<child/>\n</root>"));
    }

    #[test]
    fn structured_yaml_document_marker() {
        assert!(looks_structured("---\nkey: value\n"));
    }

    #[test]
    fn structured_high_kv_ratio_yaml() {
        let yaml = "key1: val1\nkey2: val2\nkey3: val3\n";
        assert!(looks_structured(yaml));
    }

    #[test]
    fn structured_plain_text_not_structured() {
        assert!(!looks_structured(
            "This is a normal paragraph of text with no structure."
        ));
    }

    // ── looks_like_build_progress ─────────────────────────────────────────────

    #[test]
    fn build_progress_all_progress_lines() {
        let text = "Compiling foo v1.0\nChecking bar v2.0\nFinished dev\n";
        assert!(looks_like_build_progress(text));
    }

    #[test]
    fn build_progress_with_error_is_not_progress() {
        let text = "Compiling foo v1.0\nerror[E0001]: something\n";
        assert!(!looks_like_build_progress(text));
    }

    #[test]
    fn build_progress_mixed_content_is_not_progress() {
        assert!(!looks_like_build_progress("Hello, this is output.\n"));
    }

    #[test]
    fn build_progress_empty_is_not_progress() {
        assert!(!looks_like_build_progress(""));
    }
}
