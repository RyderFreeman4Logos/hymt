use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use hymt_core::language::DocumentTranslationPolicy;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

use crate::error::CacheError;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS exec_cache (
    command TEXT NOT NULL,
    subcommand TEXT NOT NULL,
    output_hash TEXT NOT NULL,
    target_lang TEXT NOT NULL,
    source_text TEXT NOT NULL,
    translated_text TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (command, subcommand, output_hash, target_lang)
);
";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExecCacheKey {
    pub command: String,
    pub subcommand: String,
    pub output_hash: String,
    pub target_lang: String,
    /// Stable document policy label included in `output_hash` key material.
    pub document_policy: String,
}

/// Two-tier translation cache: user-private tier and a shared (read-only) tier.
///
/// Lookups search the user tier first, then the shared tier.
/// Writes always go to the specified tier only.
pub struct ExecCache {
    user_path: PathBuf,
    shared_path: PathBuf,
}

impl ExecCache {
    pub fn new(shared_path: impl AsRef<Path>) -> Self {
        Self {
            shared_path: shared_path.as_ref().to_owned(),
            user_path: default_user_path(),
        }
    }

    pub fn with_user_path(shared_path: impl AsRef<Path>, user_path: impl AsRef<Path>) -> Self {
        Self {
            shared_path: shared_path.as_ref().to_owned(),
            user_path: user_path.as_ref().to_owned(),
        }
    }

    pub fn user_path(&self) -> &Path {
        &self.user_path
    }

    pub fn shared_path(&self) -> &Path {
        &self.shared_path
    }

    /// Search user tier first, then shared tier, scoped by document policy.
    pub fn find(
        &self,
        command: &str,
        subcommand: &str,
        source_text: &str,
        target_lang: &str,
        document_policy: DocumentTranslationPolicy,
    ) -> Result<Option<String>, CacheError> {
        let key = build_key(
            command,
            subcommand,
            source_text,
            target_lang,
            document_policy,
        );
        if let Some(hit) = self.find_in_user(&key)? {
            return Ok(Some(hit));
        }
        self.find_in_shared(&key)
    }

    pub fn store_user(
        &self,
        command: &str,
        subcommand: &str,
        source_text: &str,
        target_lang: &str,
        document_policy: DocumentTranslationPolicy,
        translated_text: &str,
    ) -> Result<(), CacheError> {
        let key = build_key(
            command,
            subcommand,
            source_text,
            target_lang,
            document_policy,
        );
        if let Some(parent) = self.user_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&self.user_path)?;
        store_in_conn(&conn, &key, source_text, translated_text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&self.user_path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&self.user_path, perms)?;
        }
        Ok(())
    }

    pub fn store_shared(
        &self,
        command: &str,
        subcommand: &str,
        source_text: &str,
        target_lang: &str,
        document_policy: DocumentTranslationPolicy,
        translated_text: &str,
    ) -> Result<(), CacheError> {
        let key = build_key(
            command,
            subcommand,
            source_text,
            target_lang,
            document_policy,
        );
        if let Some(parent) = self.shared_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&self.shared_path)?;
        store_in_conn(&conn, &key, source_text, translated_text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&self.shared_path)?.permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&self.shared_path, perms)?;
        }
        Ok(())
    }

    fn find_in_user(&self, key: &ExecCacheKey) -> Result<Option<String>, CacheError> {
        if !self.user_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(&self.user_path)?;
        find_in_conn(&conn, key)
    }

    fn find_in_shared(&self, key: &ExecCacheKey) -> Result<Option<String>, CacheError> {
        if !self.shared_path.exists() {
            return Ok(None);
        }
        let conn = match Connection::open_with_flags(
            &self.shared_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };
        match find_in_conn_readonly(&conn, key) {
            Ok(v) => Ok(v),
            Err(_) => Ok(None),
        }
    }
}

/// Default path for the user-private exec cache.
pub fn default_user_path() -> PathBuf {
    home_dir().join(".cache/hymt/exec-cache.db")
}

/// SHA-256 hex digest of UTF-8-encoded text.
pub fn hash_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn build_key(
    command: &str,
    subcommand: &str,
    source_text: &str,
    target_lang: &str,
    document_policy: DocumentTranslationPolicy,
) -> ExecCacheKey {
    let document_policy = document_policy_key(document_policy);
    ExecCacheKey {
        command: command.to_owned(),
        subcommand: subcommand.to_owned(),
        output_hash: hash_text_with_document_policy(source_text, document_policy),
        target_lang: target_lang.to_owned(),
        document_policy: document_policy.to_owned(),
    }
}

fn document_policy_key(policy: DocumentTranslationPolicy) -> &'static str {
    match policy {
        DocumentTranslationPolicy::TranslateAll => "translate_all",
        DocumentTranslationPolicy::SkipHighConfidenceTargetParagraphs => {
            "skip_high_confidence_target_paragraphs"
        }
    }
}

/// Hash source text with its document policy so policy changes cannot reuse an
/// exec-cache entry created under a different translation plan.
fn hash_text_with_document_policy(source_text: &str, document_policy: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(document_policy.as_bytes());
    hasher.update([0]);
    hasher.update(source_text.as_bytes());
    hex::encode(hasher.finalize())
}

fn ensure_schema(conn: &Connection) -> Result<(), CacheError> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

fn find_in_conn(conn: &Connection, key: &ExecCacheKey) -> Result<Option<String>, CacheError> {
    ensure_schema(conn)?;
    query_translated_text(conn, key)
}

/// Like `find_in_conn` but skips schema creation (for read-only connections).
fn find_in_conn_readonly(
    conn: &Connection,
    key: &ExecCacheKey,
) -> Result<Option<String>, CacheError> {
    query_translated_text(conn, key)
}

fn query_translated_text(
    conn: &Connection,
    key: &ExecCacheKey,
) -> Result<Option<String>, CacheError> {
    let result = conn.query_row(
        "SELECT translated_text FROM exec_cache
         WHERE command = ?1 AND subcommand = ?2 AND output_hash = ?3 AND target_lang = ?4
         LIMIT 1",
        rusqlite::params![
            key.command,
            key.subcommand,
            key.output_hash,
            key.target_lang
        ],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(text) => Ok(Some(text)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(CacheError::Db(e)),
    }
}

fn store_in_conn(
    conn: &Connection,
    key: &ExecCacheKey,
    source_text: &str,
    translated_text: &str,
) -> Result<(), CacheError> {
    ensure_schema(conn)?;
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    conn.execute(
        "INSERT INTO exec_cache
             (command, subcommand, output_hash, target_lang, source_text, translated_text, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(command, subcommand, output_hash, target_lang)
         DO UPDATE SET
             source_text = excluded.source_text,
             translated_text = excluded.translated_text,
             created_at = excluded.created_at",
        rusqlite::params![
            key.command,
            key.subcommand,
            key.output_hash,
            key.target_lang,
            source_text,
            translated_text,
            now,
        ],
    )?;
    Ok(())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const DEFAULT_DOCUMENT_POLICY: DocumentTranslationPolicy =
        DocumentTranslationPolicy::SkipHighConfidenceTargetParagraphs;

    fn make_cache(tmp: &TempDir) -> ExecCache {
        let shared = tmp.path().join("shared.db");
        let user = tmp.path().join("user.db");
        ExecCache::with_user_path(&shared, &user)
    }

    #[test]
    fn test_missing_key_returns_none() {
        let tmp = TempDir::new().unwrap();
        let cache = make_cache(&tmp);
        let result = cache
            .find("ls", "-la", "some source", "en", DEFAULT_DOCUMENT_POLICY)
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_store_and_find_user() {
        let tmp = TempDir::new().unwrap();
        let cache = make_cache(&tmp);
        cache
            .store_user(
                "ls",
                "-la",
                "hello world",
                "en",
                DEFAULT_DOCUMENT_POLICY,
                "你好世界",
            )
            .unwrap();
        let found = cache
            .find("ls", "-la", "hello world", "en", DEFAULT_DOCUMENT_POLICY)
            .unwrap();
        assert_eq!(found.as_deref(), Some("你好世界"));
    }

    #[test]
    fn test_document_policy_separates_cache_entries() {
        let tmp = TempDir::new().unwrap();
        let cache = make_cache(&tmp);
        cache
            .store_user(
                "ls",
                "-la",
                "hello world",
                "en",
                DocumentTranslationPolicy::TranslateAll,
                "translation with every paragraph submitted",
            )
            .unwrap();

        assert_eq!(
            cache
                .find(
                    "ls",
                    "-la",
                    "hello world",
                    "en",
                    DocumentTranslationPolicy::TranslateAll,
                )
                .unwrap()
                .as_deref(),
            Some("translation with every paragraph submitted")
        );
        assert!(cache
            .find(
                "ls",
                "-la",
                "hello world",
                "en",
                DocumentTranslationPolicy::SkipHighConfidenceTargetParagraphs,
            )
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_store_and_find_shared() {
        let tmp = TempDir::new().unwrap();
        let cache = make_cache(&tmp);
        cache
            .store_shared(
                "git",
                "status",
                "source text",
                "zh",
                DEFAULT_DOCUMENT_POLICY,
                "源文本",
            )
            .unwrap();
        // Different user DB (no user entry), so shared is queried
        let cache2 = ExecCache::with_user_path(
            tmp.path().join("shared.db"),
            tmp.path().join("absent-user.db"),
        );
        let found = cache2
            .find(
                "git",
                "status",
                "source text",
                "zh",
                DEFAULT_DOCUMENT_POLICY,
            )
            .unwrap();
        assert_eq!(found.as_deref(), Some("源文本"));
    }

    #[test]
    fn test_user_priority_over_shared() {
        let tmp = TempDir::new().unwrap();
        let cache = make_cache(&tmp);
        cache
            .store_shared(
                "cmd",
                "sub",
                "text",
                "en",
                DEFAULT_DOCUMENT_POLICY,
                "shared-translation",
            )
            .unwrap();
        cache
            .store_user(
                "cmd",
                "sub",
                "text",
                "en",
                DEFAULT_DOCUMENT_POLICY,
                "user-translation",
            )
            .unwrap();
        let found = cache
            .find("cmd", "sub", "text", "en", DEFAULT_DOCUMENT_POLICY)
            .unwrap();
        assert_eq!(found.as_deref(), Some("user-translation"));
    }

    #[test]
    fn test_different_commands_dont_collide() {
        let tmp = TempDir::new().unwrap();
        let cache = make_cache(&tmp);
        cache
            .store_user(
                "cmd-a",
                "sub",
                "text",
                "en",
                DEFAULT_DOCUMENT_POLICY,
                "translation-a",
            )
            .unwrap();
        cache
            .store_user(
                "cmd-b",
                "sub",
                "text",
                "en",
                DEFAULT_DOCUMENT_POLICY,
                "translation-b",
            )
            .unwrap();
        assert_eq!(
            cache
                .find("cmd-a", "sub", "text", "en", DEFAULT_DOCUMENT_POLICY)
                .unwrap()
                .as_deref(),
            Some("translation-a")
        );
        assert_eq!(
            cache
                .find("cmd-b", "sub", "text", "en", DEFAULT_DOCUMENT_POLICY)
                .unwrap()
                .as_deref(),
            Some("translation-b")
        );
    }

    #[test]
    fn test_schema_auto_created_on_first_store() {
        let tmp = TempDir::new().unwrap();
        let user_path = tmp.path().join("subdir").join("user.db");
        let shared_path = tmp.path().join("shared.db");
        let cache = ExecCache::with_user_path(&shared_path, &user_path);
        // The subdir doesn't exist yet; store_user must create it
        cache
            .store_user(
                "cmd",
                "sub",
                "text",
                "en",
                DEFAULT_DOCUMENT_POLICY,
                "translation",
            )
            .unwrap();
        assert!(user_path.exists());
        let found = cache
            .find("cmd", "sub", "text", "en", DEFAULT_DOCUMENT_POLICY)
            .unwrap();
        assert_eq!(found.as_deref(), Some("translation"));
    }

    #[test]
    fn test_upsert_overwrites_existing() {
        let tmp = TempDir::new().unwrap();
        let cache = make_cache(&tmp);
        cache
            .store_user("cmd", "sub", "text", "en", DEFAULT_DOCUMENT_POLICY, "first")
            .unwrap();
        cache
            .store_user(
                "cmd",
                "sub",
                "text",
                "en",
                DEFAULT_DOCUMENT_POLICY,
                "second",
            )
            .unwrap();
        let found = cache
            .find("cmd", "sub", "text", "en", DEFAULT_DOCUMENT_POLICY)
            .unwrap();
        assert_eq!(found.as_deref(), Some("second"));
    }

    #[test]
    fn test_hash_text_is_deterministic() {
        let h1 = hash_text("hello");
        let h2 = hash_text("hello");
        assert_eq!(h1, h2);
        let h3 = hash_text("world");
        assert_ne!(h1, h3);
    }
}
