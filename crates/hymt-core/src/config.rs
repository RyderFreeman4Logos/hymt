//! Hot-reloadable TOML configuration.
//!
//! Reads from `~/.config/hymt/config.toml`, creating it with embedded defaults
//! when absent. Call `maybe_reload()` to pick up on-disk changes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use crate::error::CoreError;

pub const DEFAULT_CONFIG: &str = r#"[endpoint]
url = "http://127.0.0.1:8401/v1"
api_key = ""
model = ""

[translation]
context_window = 16384
max_output_tokens = 4096
concurrency = 1
stream = true
max_retranslation_retries = 10
config_version = 1
timeout = 600
first_chunk_priority = false

[language]
primary = "zh"
secondary = "en"

[inference]
temperature = 0.7
top_p = 0.6
top_k = 20
repetition_penalty = 1.05

[timing]
divergence_threshold = 2.0

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 2

[exec]
shared_cache_path = "/usr/local/share/hymt/cache.db"
translate_stderr = true
translate_stdout = true
skip_patterns = []
skip_commands = []

[exec.plugin]
blocklist = [
    "zstd", "gzip", "bzip2", "xz", "lz4", "rage", "age",
    "gpg", "openssl", "base64", "xxd", "od", "hexdump", "dd",
    "cp", "mv", "rsync", "docker", "podman", "hymt", "ssh", "scp"
]
"#;

const DEFAULT_BLOCKLIST: &[&str] = &[
    "zstd", "gzip", "bzip2", "xz", "lz4", "rage", "age", "gpg", "openssl", "base64", "xxd", "od",
    "hexdump", "dd", "cp", "mv", "rsync", "docker", "podman", "hymt", "ssh", "scp",
];

fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    home.join(".config").join("hymt").join("config.toml")
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(s)
    }
}

#[derive(Debug)]
struct ConfigState {
    data: toml::Table,
    mtime: Option<SystemTime>,
}

impl ConfigState {
    fn empty() -> Self {
        Self {
            data: toml::Table::new(),
            mtime: None,
        }
    }
}

/// Hot-reloadable TOML configuration.
#[derive(Debug, Clone)]
pub struct HotConfig {
    path: PathBuf,
    state: Arc<RwLock<ConfigState>>,
}

impl HotConfig {
    /// Opens the default config (`~/.config/hymt/config.toml`), creating it if absent.
    pub fn new() -> Result<Self, CoreError> {
        Self::from_path(default_config_path())
    }

    /// Opens the config at `path`, creating it if absent.
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let path = path.into();
        let cfg = Self {
            path,
            state: Arc::new(RwLock::new(ConfigState::empty())),
        };
        cfg.ensure_exists()?;
        cfg.load_from_disk()?;
        Ok(cfg)
    }

    /// Path of the underlying config file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reloads from disk if the file's mtime changed since the last load.
    ///
    /// Returns `true` when a reload occurred.
    pub fn maybe_reload(&self) -> Result<bool, CoreError> {
        let current_mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        {
            let state = self.state.read().unwrap();
            if state.mtime == current_mtime {
                return Ok(false);
            }
        }
        self.load_from_disk()?;
        Ok(true)
    }

    /// Returns the raw text of the config file (reloads if modified).
    pub fn show(&self) -> Result<String, CoreError> {
        self.maybe_reload()?;
        std::fs::read_to_string(&self.path).map_err(CoreError::Io)
    }

    // ── endpoint ────────────────────────────────────────────────────────────

    pub fn endpoint_url(&self) -> String {
        let s = self.get_str("endpoint", "url", "http://127.0.0.1:8401/v1");
        s.trim_end_matches('/').to_owned()
    }

    pub fn api_key(&self) -> String {
        self.get_str("endpoint", "api_key", "")
    }

    pub fn model(&self) -> String {
        self.get_str("endpoint", "model", "")
    }

    // ── translation ─────────────────────────────────────────────────────────

    pub fn context_window(&self) -> u32 {
        self.get_positive_u32("translation", "context_window", 16384)
    }

    pub fn max_output_tokens(&self) -> u32 {
        self.get_positive_u32("translation", "max_output_tokens", 4096)
    }

    pub fn concurrency(&self) -> u32 {
        self.get_positive_u32("translation", "concurrency", 1)
    }

    pub fn stream(&self) -> bool {
        self.get_bool("translation", "stream", true)
    }

    pub fn max_retranslation_retries(&self) -> u32 {
        self.get_positive_u32("translation", "max_retranslation_retries", 10)
    }

    pub fn config_version(&self) -> u32 {
        self.get_positive_u32("translation", "config_version", 1)
    }

    pub fn timeout(&self) -> f64 {
        self.get_number_as_f64("translation", "timeout", 600.0)
    }

    /// When true, chunk 0 is translated alone before remaining chunks run in parallel.
    ///
    /// This gives the first output segment priority GPU access so callers can
    /// display it immediately while the rest of the document is still being
    /// translated.  Defaults to `false` (all missing chunks are parallelised).
    pub fn first_chunk_priority(&self) -> bool {
        self.get_bool("translation", "first_chunk_priority", false)
    }

    // ── language ────────────────────────────────────────────────────────────

    pub fn primary_lang(&self) -> String {
        self.get_str("language", "primary", "zh")
    }

    pub fn secondary_lang(&self) -> String {
        self.get_str("language", "secondary", "en")
    }

    // ── inference ───────────────────────────────────────────────────────────

    pub fn temperature(&self) -> f64 {
        self.get_number_as_f64("inference", "temperature", 0.7)
    }

    pub fn top_p(&self) -> f64 {
        self.get_number_as_f64("inference", "top_p", 0.6)
    }

    pub fn top_k(&self) -> u32 {
        self.get_positive_u32("inference", "top_k", 20)
    }

    pub fn repetition_penalty(&self) -> f64 {
        self.get_number_as_f64("inference", "repetition_penalty", 1.05)
    }

    // ── timing ──────────────────────────────────────────────────────────────

    /// Minimum ratio threshold for divergence detection (must be > 1.0; defaults to 2.0).
    pub fn timing_divergence_threshold(&self) -> f64 {
        let v = self.get_number_as_f64("timing", "divergence_threshold", 2.0);
        if v > 1.0 {
            v
        } else {
            2.0
        }
    }

    // ── completeness ────────────────────────────────────────────────────────

    pub fn completeness_zh_to_en_min_ratio(&self) -> f64 {
        let v = self.get_number_as_f64("completeness", "zh_to_en_min_ratio", 0.3);
        if v > 0.0 {
            v
        } else {
            0.3
        }
    }

    pub fn completeness_en_to_zh_min_ratio(&self) -> f64 {
        let v = self.get_number_as_f64("completeness", "en_to_zh_min_ratio", 0.3);
        if v > 0.0 {
            v
        } else {
            0.3
        }
    }

    pub fn completeness_min_paragraph_ratio(&self) -> f64 {
        let v = self.get_number_as_f64("completeness", "min_paragraph_ratio", 0.5);
        if v > 0.0 {
            v
        } else {
            0.5
        }
    }

    pub fn completeness_max_retries(&self) -> u32 {
        self.get_non_negative_u32("completeness", "max_retries", 2)
    }

    // ── exec ────────────────────────────────────────────────────────────────

    pub fn exec_shared_cache_path(&self) -> PathBuf {
        let s = self.get_str(
            "exec",
            "shared_cache_path",
            "/usr/local/share/hymt/cache.db",
        );
        expand_tilde(&s)
    }

    pub fn exec_translate_stderr(&self) -> bool {
        self.get_bool("exec", "translate_stderr", true)
    }

    /// Returns the `translate_stdout` setting; `"auto"` is treated as `true`.
    pub fn exec_translate_stdout(&self) -> bool {
        let state = self.state.read().unwrap();
        match state
            .data
            .get("exec")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("translate_stdout"))
        {
            Some(toml::Value::Boolean(b)) => *b,
            Some(toml::Value::String(s)) if s == "auto" => true,
            _ => true,
        }
    }

    pub fn exec_skip_patterns(&self) -> Vec<String> {
        self.get_string_vec("exec", "skip_patterns")
    }

    pub fn exec_skip_commands(&self) -> Vec<String> {
        self.get_string_vec("exec", "skip_commands")
    }

    pub fn exec_plugin_blocklist(&self) -> Vec<String> {
        let state = self.state.read().unwrap();
        state
            .data
            .get("exec")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("plugin"))
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("blocklist"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| DEFAULT_BLOCKLIST.iter().map(|s| s.to_string()).collect())
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn load_from_disk(&self) -> Result<(), CoreError> {
        let content = std::fs::read_to_string(&self.path)?;
        let data: toml::Table = toml::from_str(&content)
            .map_err(|e| CoreError::Config(format!("{}: {}", self.path.display(), e)))?;
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        let mut state = self.state.write().unwrap();
        state.data = data;
        state.mtime = mtime;
        Ok(())
    }

    fn ensure_exists(&self) -> Result<(), CoreError> {
        if self.path.exists() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, DEFAULT_CONFIG)?;
        Ok(())
    }

    fn section_value(&self, section: &str, key: &str) -> Option<toml::Value> {
        let state = self.state.read().unwrap();
        state
            .data
            .get(section)
            .and_then(|v| v.as_table())
            .and_then(|t| t.get(key))
            .cloned()
    }

    fn get_str(&self, section: &str, key: &str, default: &str) -> String {
        self.section_value(section, key)
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| default.to_owned())
    }

    fn get_bool(&self, section: &str, key: &str, default: bool) -> bool {
        self.section_value(section, key)
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    }

    fn get_positive_u32(&self, section: &str, key: &str, default: u32) -> u32 {
        self.section_value(section, key)
            .and_then(|v| v.as_integer())
            .filter(|&n| n > 0)
            .map(|n| n as u32)
            .unwrap_or(default)
    }

    fn get_non_negative_u32(&self, section: &str, key: &str, default: u32) -> u32 {
        self.section_value(section, key)
            .and_then(|v| v.as_integer())
            .filter(|&n| n >= 0)
            .map(|n| n as u32)
            .unwrap_or(default)
    }

    fn get_number_as_f64(&self, section: &str, key: &str, default: f64) -> f64 {
        match self.section_value(section, key) {
            Some(toml::Value::Float(f)) => f,
            Some(toml::Value::Integer(i)) => i as f64,
            _ => default,
        }
    }

    fn get_string_vec(&self, section: &str, key: &str) -> Vec<String> {
        self.section_value(section, key)
            .and_then(|v| v.as_array().cloned())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_config_path(tag: &str) -> PathBuf {
        let unique = format!(
            "{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let dir = std::env::temp_dir().join(format!("hymt-test-{}", unique));
        fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn defaults_when_file_absent() {
        let path = temp_config_path("defaults");
        let cfg = HotConfig::from_path(&path).unwrap();

        assert_eq!(cfg.endpoint_url(), "http://127.0.0.1:8401/v1");
        assert_eq!(cfg.concurrency(), 1);
        assert_eq!(cfg.primary_lang(), "zh");
        assert_eq!(cfg.secondary_lang(), "en");
        assert_eq!(cfg.stream(), true);
        assert_eq!(cfg.max_retranslation_retries(), 10);
        assert!((cfg.timeout() - 600.0).abs() < f64::EPSILON);
        assert!((cfg.completeness_zh_to_en_min_ratio() - 0.3).abs() < f64::EPSILON);
        assert!((cfg.completeness_en_to_zh_min_ratio() - 0.3).abs() < f64::EPSILON);
        assert!((cfg.completeness_min_paragraph_ratio() - 0.5).abs() < f64::EPSILON);
        assert_eq!(cfg.completeness_max_retries(), 2);
    }

    #[test]
    fn creates_default_file_when_absent() {
        let path = temp_config_path("create");
        assert!(!path.exists());
        let _cfg = HotConfig::from_path(&path).unwrap();
        assert!(path.exists());
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[endpoint]"));
    }

    #[test]
    fn reads_custom_values() {
        let path = temp_config_path("custom");
        fs::write(
            &path,
            r#"
[endpoint]
url = "http://example.com:9000/v1/"
model = "hy-mt2"

[translation]
concurrency = 4
timeout = 120

[language]
primary = "en"
secondary = "zh"
"#,
        )
        .unwrap();

        let cfg = HotConfig::from_path(&path).unwrap();
        assert_eq!(cfg.endpoint_url(), "http://example.com:9000/v1");
        assert_eq!(cfg.model(), "hy-mt2");
        assert_eq!(cfg.concurrency(), 4);
        assert!((cfg.timeout() - 120.0).abs() < f64::EPSILON);
        assert_eq!(cfg.primary_lang(), "en");
        assert_eq!(cfg.secondary_lang(), "zh");
        // Unset fields keep defaults
        assert_eq!(cfg.max_output_tokens(), 4096);
    }

    #[test]
    fn endpoint_url_strips_trailing_slash() {
        let path = temp_config_path("slash");
        fs::write(
            &path,
            r#"[endpoint]
url = "http://localhost:8401/v1/""#,
        )
        .unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert_eq!(cfg.endpoint_url(), "http://localhost:8401/v1");
    }

    #[test]
    fn integer_timeout_parsed_as_float() {
        let path = temp_config_path("int_timeout");
        fs::write(&path, "[translation]\ntimeout = 300").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert!((cfg.timeout() - 300.0).abs() < f64::EPSILON);
    }

    #[test]
    fn invalid_toml_returns_config_error() {
        let path = temp_config_path("invalid");
        fs::write(&path, "not valid toml = = =").unwrap();
        let result = HotConfig::from_path(&path);
        assert!(matches!(result, Err(CoreError::Config(_))));
    }

    #[test]
    fn maybe_reload_detects_changes() {
        let path = temp_config_path("reload");
        fs::write(&path, "[translation]\nconcurrency = 1").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert_eq!(cfg.concurrency(), 1);

        // Overwrite and force a different mtime
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, "[translation]\nconcurrency = 8").unwrap();
        cfg.maybe_reload().unwrap();
        assert_eq!(cfg.concurrency(), 8);
    }

    #[test]
    fn positive_int_rejects_zero_and_negative() {
        let path = temp_config_path("pos_int");
        fs::write(&path, "[translation]\nconcurrency = -1").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        // -1 is not positive, falls back to default
        assert_eq!(cfg.concurrency(), 1);
    }

    #[test]
    fn timing_divergence_threshold_min_enforced() {
        let path = temp_config_path("timing");
        fs::write(&path, "[timing]\ndivergence_threshold = 0.5").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        // 0.5 ≤ 1.0, falls back to default 2.0
        assert!((cfg.timing_divergence_threshold() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn first_chunk_priority_defaults_false() {
        let path = temp_config_path("fcp_default");
        let cfg = HotConfig::from_path(&path).unwrap();
        assert!(!cfg.first_chunk_priority());
    }

    #[test]
    fn first_chunk_priority_reads_true() {
        let path = temp_config_path("fcp_true");
        fs::write(&path, "[translation]\nfirst_chunk_priority = true").unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert!(cfg.first_chunk_priority());
    }

    #[test]
    fn exec_plugin_blocklist_defaults() {
        let path = temp_config_path("blocklist");
        let cfg = HotConfig::from_path(&path).unwrap();
        let bl = cfg.exec_plugin_blocklist();
        assert!(bl.contains(&"docker".to_owned()));
        assert!(bl.contains(&"hymt".to_owned()));
        assert!(bl.contains(&"ssh".to_owned()));
    }

    #[test]
    fn exec_plugin_blocklist_custom() {
        let path = temp_config_path("blocklist_custom");
        fs::write(
            &path,
            r#"[exec.plugin]
blocklist = ["foo", "bar"]"#,
        )
        .unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        let bl = cfg.exec_plugin_blocklist();
        assert_eq!(bl, vec!["foo".to_owned(), "bar".to_owned()]);
    }
}
