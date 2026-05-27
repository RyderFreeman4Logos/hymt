//! Zsh plugin generation and installation.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};

const PLUGIN_HEADER: &str = r#"# hymt-exec zsh plugin — auto-generated, do not edit manually
# Translates the output of shell commands via `hymt exec`.

"#;

/// Render the zsh plugin script with the given blocklist.
pub fn render_zsh_plugin(blocklist: &[String]) -> String {
    let blocklist_str = if blocklist.is_empty() {
        String::new()
    } else {
        blocklist
            .iter()
            .map(|s| shell_quote(s))
            .collect::<Vec<_>>()
            .join(" ")
    };

    format!(
        r#"{header}# Blocklist: commands whose output is NOT translated.
_HYMT_EXEC_BLOCKLIST=({blocklist})

# Detect if running inside an agent/script (no translation in non-interactive contexts).
_hymt_is_agent() {{
  [[ -n "$CLAUDE_SESSION_ID" || -n "$CODEX_SESSION" || -n "$AIDER_SESSION" ]] && return 0
  [[ -n "$CI" || -n "$GITHUB_ACTIONS" || -n "$BUILDKITE" ]] && return 0
  return 1
}}

# Check if a command is in the blocklist.
_hymt_is_blocked() {{
  local cmd="$1"
  for blocked in "${{_HYMT_EXEC_BLOCKLIST[@]}}"; do
    [[ "$cmd" == "$blocked" ]] && return 0
  done
  return 1
}}

# Translate function: wraps a command and pipes output through hymt exec.
t() {{
  if _hymt_is_agent; then
    command "$@"
    return
  fi
  local cmd="${{1:-}}"
  if [[ -z "$cmd" ]]; then
    echo "Usage: t <command> [args...]" >&2
    return 1
  fi
  if _hymt_is_blocked "$cmd"; then
    command "$@"
    return
  fi
  hymt exec -- "$@"
}}
"#,
        header = PLUGIN_HEADER,
        blocklist = blocklist_str,
    )
}

fn shell_quote(s: &str) -> String {
    // Single-quote the string, escaping embedded single quotes as '\''
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Result of installing the zsh plugin.
pub struct ZshPluginResult {
    pub plugin_path: PathBuf,
    pub zshrc_updated: bool,
}

/// Write the zsh plugin file and ensure ~/.zshrc sources it.
///
/// If `update` is false and the plugin file already exists, this is a no-op.
pub fn install_zsh_plugin(blocklist: &[String], update: bool) -> Result<ZshPluginResult> {
    let plugin_dir = dirs_plugin_dir()?;
    std::fs::create_dir_all(&plugin_dir)
        .with_context(|| format!("creating plugin dir {}", plugin_dir.display()))?;

    let plugin_path = plugin_dir.join("hymt-exec.zsh");

    if plugin_path.exists() && !update {
        return Ok(ZshPluginResult {
            plugin_path,
            zshrc_updated: false,
        });
    }

    let content = render_zsh_plugin(blocklist);
    std::fs::write(&plugin_path, &content)
        .with_context(|| format!("writing plugin to {}", plugin_path.display()))?;

    let zshrc_updated = ensure_zshrc_sources(&plugin_path)?;

    Ok(ZshPluginResult {
        plugin_path,
        zshrc_updated,
    })
}

fn dirs_plugin_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("HOME not set")?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("hymt"))
}

fn ensure_zshrc_sources(plugin_path: &std::path::Path) -> Result<bool> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("HOME not set")?;
    let zshrc = PathBuf::from(home).join(".zshrc");

    let source_line = format!("source {}", plugin_path.display());

    // Read existing content; if ~/.zshrc doesn't exist, treat as empty.
    let existing = if zshrc.exists() {
        std::fs::read_to_string(&zshrc).with_context(|| format!("reading {}", zshrc.display()))?
    } else {
        String::new()
    };

    if existing.lines().any(|l| l.trim() == source_line) {
        return Ok(false);
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&zshrc)
        .with_context(|| format!("opening {}", zshrc.display()))?;
    writeln!(file, "\n{source_line}").with_context(|| format!("writing to {}", zshrc.display()))?;

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_empty_blocklist() {
        let script = render_zsh_plugin(&[]);
        assert!(script.contains("_HYMT_EXEC_BLOCKLIST=()"));
        assert!(script.contains("hymt exec -- \"$@\""));
    }

    #[test]
    fn render_blocklist_quoted() {
        let script = render_zsh_plugin(&["git".to_owned(), "vim".to_owned()]);
        assert!(script.contains("'git' 'vim'"));
    }

    #[test]
    fn shell_quote_simple() {
        assert_eq!(shell_quote("git"), "'git'");
    }

    #[test]
    fn shell_quote_with_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
