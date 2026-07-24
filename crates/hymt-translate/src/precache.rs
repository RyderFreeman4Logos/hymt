//! Background pre-caching: parse shell history and pre-translate man/help output.
//!
//! Reads bash, zsh, and fish history files to discover recently-used commands,
//! then translates their man pages and `--help` output into the target language
//! so that subsequent `hymt exec` invocations hit the cache immediately.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use hymt_cache::history::HistoryDB;
use hymt_cache::ExecCache;
use hymt_client::TranslationClient;
use hymt_core::config::HotConfig;
use hymt_core::templates::PromptOpts;
use hymt_segment::Segmenter;

use crate::docs::capture_man;
use crate::exec_wrapper::{decode_for_translation, translate_cached};
use crate::translate::TranslationCtx;

// ── Constants ─────────────────────────────────────────────────────────────────

const RECENT_HISTORY_LINE_LIMIT: usize = 1000;
const RECENT_HISTORY_COMMAND_LIMIT: usize = 100;

const SHELL_BUILTINS: &[&str] = &[
    "alias", "bg", "cd", "dirs", "disown", "echo", "eval", "exec", "exit", "export", "fg", "hash",
    "history", "jobs", "popd", "pushd", "pwd", "read", "set", "shift", "source", "test", "trap",
    "type", "ulimit", "unalias", "unset", "wait",
];

// Options for `sudo`/`doas` that consume the next token as an argument.
const SUDO_OPTIONS_WITH_ARGS: &[&str] = &[
    "-C",
    "-D",
    "-R",
    "-T",
    "-g",
    "-p",
    "-u",
    "--chdir",
    "--chroot",
    "--close-from",
    "--command-timeout",
    "--group",
    "--prompt",
    "--user",
];

// ── Public types ──────────────────────────────────────────────────────────────

/// Result of a `run_precache` call.
pub struct PrecacheSummary {
    /// Total number of commands discovered.
    pub total: usize,
    /// Number of items (man + help) successfully translated.
    pub translated: usize,
    /// Number of items that failed to translate.
    pub failed: usize,
}

// ── Shell history parsing (pub for unit tests) ────────────────────────────────

/// Parse bash history content and return command names, most-recent first.
///
/// Timestamp lines (`#<digits>`) are skipped.
pub fn parse_bash_history(content: &str) -> Vec<String> {
    let lines: Vec<&str> = content.lines().collect();
    lines
        .iter()
        .rev()
        .take(RECENT_HISTORY_LINE_LIMIT)
        .filter_map(|line| extract_first_command(line))
        .collect()
}

/// Parse zsh extended history content and return command names, most-recent first.
///
/// Handles both `: <timestamp>:<elapsed>;<command>` lines and plain command lines.
pub fn parse_zsh_history(content: &str) -> Vec<String> {
    let lines: Vec<&str> = content.lines().collect();
    lines
        .iter()
        .rev()
        .take(RECENT_HISTORY_LINE_LIMIT)
        .filter_map(|line| {
            let line = line.trim();
            let cmd_part = if line.starts_with(": ") {
                line.split_once(';').map(|(_, r)| r).unwrap_or(line)
            } else {
                line
            };
            extract_first_command(cmd_part)
        })
        .collect()
}

/// Parse fish history content and return command names, most-recent first.
///
/// Only `- cmd: <command>` lines are considered.
pub fn parse_fish_history(content: &str) -> Vec<String> {
    let lines: Vec<&str> = content.lines().collect();
    lines
        .iter()
        .rev()
        .take(RECENT_HISTORY_LINE_LIMIT)
        .filter_map(|line| {
            let line = line.trim();
            let cmd_part = line.strip_prefix("- cmd:")?;
            extract_first_command(cmd_part.trim())
        })
        .collect()
}

// ── Command extraction ────────────────────────────────────────────────────────

fn extract_first_command(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // Bash timestamp: #<digits>
    if line.starts_with('#') && line[1..].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // Zsh extended history in raw form: ": timestamp:0;command"
    let cmd_part = if line.starts_with(": ") && line.contains(';') {
        line.split_once(';').map(|(_, r)| r).unwrap_or(line)
    } else {
        line
    };
    // Fish history in raw form: "- cmd: command"
    let cmd_part = if let Some(rest) = cmd_part.strip_prefix("- cmd:") {
        rest.trim()
    } else {
        cmd_part
    };
    let tokens: Vec<&str> = cmd_part.split_whitespace().collect();
    first_command_token(&tokens)
}

fn first_command_token(tokens: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        if token.is_empty() || token.starts_with('-') {
            i += 1;
            continue;
        }
        // VAR=value environment assignments
        if token.contains('=') && !token.contains('/') {
            i += 1;
            continue;
        }
        match token {
            "sudo" | "doas" => {
                i = skip_options_with_args(tokens, i + 1, SUDO_OPTIONS_WITH_ARGS);
                continue;
            }
            "command" | "builtin" | "noglob" => {
                i += 1;
                continue;
            }
            "env" => {
                i += 1;
                while i < tokens.len() && tokens[i].contains('=') {
                    i += 1;
                }
                continue;
            }
            cmd if SHELL_BUILTINS.contains(&cmd) => return None,
            _ => {
                // Strip path prefix: /usr/bin/git → git
                let name = Path::new(token)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| token.to_owned());
                return Some(name);
            }
        }
    }
    None
}

fn skip_options_with_args(tokens: &[&str], mut i: usize, opts_with_args: &[&str]) -> usize {
    while i < tokens.len() && tokens[i].starts_with('-') {
        let opt = tokens[i];
        i += 1;
        if opt == "--" {
            break;
        }
        let opt_name = opt.split('=').next().unwrap_or("");
        if opts_with_args.contains(&opt_name) && !opt.contains('=') && i < tokens.len() {
            i += 1; // consume the argument
        }
    }
    i
}

// ── Shell history file discovery ──────────────────────────────────────────────

fn shell_history_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(h) = std::env::var("HISTFILE") {
        if !h.is_empty() {
            paths.push(PathBuf::from(h));
        }
    }
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    paths.push(home.join(".zsh_history"));
    paths.push(home.join(".bash_history"));
    paths.push(
        home.join(".local")
            .join("share")
            .join("fish")
            .join("fish_history"),
    );

    // deduplicate while preserving order
    let mut seen = HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
    paths
}

fn read_history_commands(path: &Path) -> Vec<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let is_fish = file_name == "fish_history"
        || path
            .components()
            .any(|c| c.as_os_str().to_str() == Some("fish"));

    if is_fish {
        parse_fish_history(&content)
    } else if file_name == ".zsh_history" {
        parse_zsh_history(&content)
    } else {
        parse_bash_history(&content)
    }
}

// ── Command resolution ────────────────────────────────────────────────────────

fn resolve_command_on_path(name: &str) -> bool {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|dir| PathBuf::from(dir).join(name).is_file())
}

// ── Discovery ─────────────────────────────────────────────────────────────────

fn discover_recent_commands(config: &HotConfig) -> Vec<String> {
    let skip: HashSet<String> = config
        .exec_skip_commands()
        .into_iter()
        .chain(config.exec_plugin_blocklist())
        .collect();

    let mut commands: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    'outer: for path in shell_history_paths() {
        for cmd in read_history_commands(&path) {
            if skip.contains(&cmd) || seen.contains(&cmd) {
                continue;
            }
            if resolve_command_on_path(&cmd) {
                seen.insert(cmd.clone());
                commands.push(cmd);
                if commands.len() >= RECENT_HISTORY_COMMAND_LIMIT {
                    break 'outer;
                }
            }
        }
    }
    commands
}

// ── Help text capture ─────────────────────────────────────────────────────────

fn capture_help(cmd: &str) -> Option<String> {
    for flag in &["--help", "-h"] {
        let Ok(output) = std::process::Command::new(cmd).arg(flag).output() else {
            continue;
        };
        // Combine stdout + stderr (many tools write help to stderr)
        let combined: Vec<u8> = output
            .stdout
            .iter()
            .chain(&output.stderr)
            .copied()
            .collect();
        if let Some(text) = decode_for_translation(&combined) {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

// ── run_precache ──────────────────────────────────────────────────────────────

/// Discover recently-used commands from shell history and pre-translate their
/// man pages and `--help` output into `target_lang`.
pub async fn run_precache(
    target_lang: &str,
    config: &HotConfig,
    client: &TranslationClient,
    segmenter: &Segmenter,
    history: &HistoryDB,
    _explicit_target: bool,
    prompt_opts: &PromptOpts,
) -> Result<PrecacheSummary> {
    let commands = discover_recent_commands(config);
    eprintln!("hymt precache: {} commands discovered", commands.len());
    let total = commands.len();
    let mut translated = 0usize;
    let mut failed = 0usize;
    let tctx = TranslationCtx {
        config,
        client,
        segmenter,
        history,
    };

    for cmd in &commands {
        // Translate man page
        match capture_man(&[cmd.as_str()]) {
            Ok(text) if !text.trim().is_empty() => {
                let cache = ExecCache::new(config.exec_shared_cache_path());
                match translate_cached("man", cmd, &text, target_lang, &cache, prompt_opts, &tctx)
                    .await
                {
                    Ok(_) => {
                        translated += 1;
                        eprintln!("hymt precache: translated man {cmd}");
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("hymt precache: failed man {cmd}: {e}");
                    }
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("hymt precache: skipped man {cmd}: {e}"),
        }

        // Translate --help output
        if let Some(text) = capture_help(cmd) {
            let cache = ExecCache::new(config.exec_shared_cache_path());
            match translate_cached(
                cmd,
                "--help",
                &text,
                target_lang,
                &cache,
                prompt_opts,
                &tctx,
            )
            .await
            {
                Ok(_) => {
                    translated += 1;
                    eprintln!("hymt precache: translated {cmd} --help");
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("hymt precache: failed {cmd} --help: {e}");
                }
            }
        }
    }

    Ok(PrecacheSummary {
        total,
        translated,
        failed,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_bash_history ────────────────────────────────────────────────────

    #[test]
    fn bash_plain_command() {
        let content = "git status\n";
        let result = parse_bash_history(content);
        assert_eq!(result, vec!["git"]);
    }

    #[test]
    fn bash_skips_timestamp_lines() {
        let content = "#1716000000\ngit log\n#1716000001\ncargo build\n";
        let result = parse_bash_history(content);
        assert_eq!(result, vec!["cargo", "git"]);
    }

    #[test]
    fn bash_skips_shell_builtins() {
        let content = "cd /tmp\necho hello\ngit status\n";
        let result = parse_bash_history(content);
        assert_eq!(result, vec!["git"]);
    }

    #[test]
    fn bash_extracts_sudo_target() {
        let content = "sudo apt install foo\n";
        let result = parse_bash_history(content);
        assert_eq!(result, vec!["apt"]);
    }

    #[test]
    fn bash_extracts_sudo_with_user_flag() {
        // sudo -u user command
        let content = "sudo -u root systemctl restart nginx\n";
        let result = parse_bash_history(content);
        assert_eq!(result, vec!["systemctl"]);
    }

    #[test]
    fn bash_strips_path_prefix() {
        let content = "/usr/bin/git status\n";
        let result = parse_bash_history(content);
        assert_eq!(result, vec!["git"]);
    }

    #[test]
    fn bash_skips_env_assignments() {
        let content = "RUST_LOG=debug cargo test\n";
        let result = parse_bash_history(content);
        assert_eq!(result, vec!["cargo"]);
    }

    #[test]
    fn bash_empty_content_gives_empty() {
        assert!(parse_bash_history("").is_empty());
    }

    #[test]
    fn bash_most_recent_first() {
        let content = "git\ncargo\nrustc\n";
        let result = parse_bash_history(content);
        // reversed: rustc, cargo, git
        assert_eq!(result[0], "rustc");
        assert_eq!(result[1], "cargo");
        assert_eq!(result[2], "git");
    }

    // ── parse_zsh_history ─────────────────────────────────────────────────────

    #[test]
    fn zsh_extended_format() {
        let content = ": 1716000000:0;git status\n";
        let result = parse_zsh_history(content);
        assert_eq!(result, vec!["git"]);
    }

    #[test]
    fn zsh_plain_format() {
        let content = "cargo build\n";
        let result = parse_zsh_history(content);
        assert_eq!(result, vec!["cargo"]);
    }

    #[test]
    fn zsh_multiple_entries_most_recent_first() {
        let content = ": 1:0;git\n: 2:0;cargo\n: 3:0;rustc\n";
        let result = parse_zsh_history(content);
        assert_eq!(result[0], "rustc");
    }

    #[test]
    fn zsh_extracts_sudo_target() {
        let content = ": 1:0;sudo systemctl status nginx\n";
        let result = parse_zsh_history(content);
        assert_eq!(result, vec!["systemctl"]);
    }

    // ── parse_fish_history ────────────────────────────────────────────────────

    #[test]
    fn fish_extracts_cmd_lines() {
        let content = "- cmd: git log\n  when: 1716000000\n";
        let result = parse_fish_history(content);
        assert_eq!(result, vec!["git"]);
    }

    #[test]
    fn fish_skips_non_cmd_lines() {
        let content = "  when: 1716000000\npaths:\n- /usr/bin\n";
        let result = parse_fish_history(content);
        assert!(result.is_empty());
    }

    #[test]
    fn fish_most_recent_first() {
        let content = "- cmd: git\n  when: 1\n- cmd: cargo\n  when: 2\n";
        let result = parse_fish_history(content);
        assert_eq!(result[0], "cargo");
    }

    // ── extract_first_command ─────────────────────────────────────────────────

    #[test]
    fn extract_handles_env_command() {
        // env VAR=val cmd
        assert_eq!(
            extract_first_command("env RUST_LOG=debug cargo test"),
            Some("cargo".to_owned())
        );
    }

    #[test]
    fn extract_handles_command_builtin() {
        assert_eq!(
            extract_first_command("command git status"),
            Some("git".to_owned())
        );
    }

    #[test]
    fn extract_empty_line_is_none() {
        assert_eq!(extract_first_command(""), None);
        assert_eq!(extract_first_command("   "), None);
    }

    #[test]
    fn extract_builtin_is_none() {
        assert_eq!(extract_first_command("cd /tmp"), None);
    }
}
