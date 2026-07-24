use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::CacheError;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    finished_at TEXT NOT NULL,
    duration_seconds REAL NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    segments INTEGER NOT NULL,
    concurrency INTEGER NOT NULL,
    source_lang TEXT,
    target_lang TEXT NOT NULL,
    template_type TEXT NOT NULL,
    model TEXT,
    tokens_per_second REAL NOT NULL,
    input_chars INTEGER NOT NULL,
    output_chars INTEGER NOT NULL,
    output_text TEXT,
    input_hash TEXT,
    config_version INTEGER DEFAULT 1,
    profile_id TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS segment_cache (
    content_hash TEXT NOT NULL,
    target_lang TEXT NOT NULL,
    template_type TEXT NOT NULL,
    options_hash TEXT NOT NULL DEFAULT '',
    profile_id TEXT NOT NULL DEFAULT '',
    translated_text TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (content_hash, target_lang, template_type, options_hash, profile_id)
);
";

#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub id: Option<i64>,
    pub started_at: String,
    pub finished_at: String,
    pub duration_seconds: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub segments: i64,
    pub concurrency: i64,
    pub source_lang: Option<String>,
    pub target_lang: String,
    pub template_type: String,
    pub model: Option<String>,
    /// Stable endpoint model-profile identifier pinned for this translation.
    pub profile_id: String,
    pub tokens_per_second: f64,
    pub input_chars: i64,
    pub output_chars: i64,
    pub output_text: Option<String>,
    pub input_hash: Option<String>,
    pub config_version: i64,
}

/// Cache-key dimensions shared by every segment in one translation session.
///
/// The profile ID is deliberately part of this scope so results generated with
/// different tokenizers or generation defaults cannot collide.
#[derive(Debug, Clone, Copy)]
pub struct SegmentCacheScope<'a> {
    pub target_lang: &'a str,
    pub template_type: &'a str,
    pub options_hash: &'a str,
    pub profile_id: &'a str,
}

#[derive(Debug, Clone)]
pub struct TranslationPreview {
    pub position: usize,
    pub id: i64,
    pub finished_at: String,
    pub target_lang: String,
    pub template_type: String,
    pub output_chars: i64,
    pub preview: String,
}

#[derive(Debug, Clone)]
pub struct PerformanceStats {
    pub count: usize,
    pub avg_tokens_per_second: f64,
    pub median_tokens_per_second: f64,
    pub p5_tokens_per_second: f64,
    pub p95_tokens_per_second: f64,
    pub avg_output_tokens_per_segment: f64,
    pub total_duration_seconds: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct DurationEstimate {
    pub stats: PerformanceStats,
    /// Estimated wall-clock seconds. Always > 0.0 when present.
    pub seconds: f64,
    pub concurrency: i64,
    pub estimated_output_tokens: f64,
    pub versions_used: Vec<i64>,
}

pub fn history_path() -> PathBuf {
    home_dir().join(".local/share/hymt/history.db")
}

/// Format a duration (seconds) as a human-readable string, e.g. "2m05s".
pub fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "0s".to_owned();
    }
    let total = seconds.round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if hours > 0 {
        format!("{}h{:02}m{:02}s", hours, minutes, secs)
    } else if minutes > 0 {
        format!("{}m{:02}s", minutes, secs)
    } else {
        format!("{}s", secs)
    }
}

pub struct HistoryDB {
    path: PathBuf,
}

impl HistoryDB {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_owned(),
        }
    }
}

impl Default for HistoryDB {
    fn default() -> Self {
        Self {
            path: history_path(),
        }
    }
}

impl HistoryDB {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn insert_task(&self, record: &TaskRecord) -> Result<(), CacheError> {
        let conn = self.connect_create()?;
        ensure_schema(&conn)?;
        conn.execute(
            "INSERT INTO tasks (
                started_at, finished_at, duration_seconds,
                input_tokens, output_tokens, segments, concurrency,
                source_lang, target_lang, template_type, model,
                tokens_per_second, input_chars, output_chars,
                output_text, input_hash, config_version, profile_id
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            rusqlite::params![
                record.started_at,
                record.finished_at,
                record.duration_seconds,
                record.input_tokens,
                record.output_tokens,
                record.segments,
                record.concurrency,
                record.source_lang,
                record.target_lang,
                record.template_type,
                record.model,
                record.tokens_per_second,
                record.input_chars,
                record.output_chars,
                record.output_text,
                record.input_hash,
                record.config_version,
                record.profile_id,
            ],
        )?;
        Ok(())
    }

    pub fn find_segment_cached(
        &self,
        content_hash: &str,
        scope: SegmentCacheScope<'_>,
    ) -> Result<Option<String>, CacheError> {
        let conn = match self.connect_if_exists()? {
            Some(c) => c,
            None => return Ok(None),
        };
        ensure_schema(&conn)?;
        let result = conn.query_row(
            "SELECT translated_text FROM segment_cache
             WHERE content_hash = ?1 AND target_lang = ?2
               AND template_type = ?3 AND options_hash = ?4 AND profile_id = ?5
             LIMIT 1",
            rusqlite::params![
                content_hash,
                scope.target_lang,
                scope.template_type,
                scope.options_hash,
                scope.profile_id
            ],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(text) => Ok(Some(text)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CacheError::Db(e)),
        }
    }

    /// Return the subset of `content_hashes` that exist in the segment cache.
    pub fn find_cached_segment_hashes(
        &self,
        content_hashes: &[&str],
        scope: SegmentCacheScope<'_>,
    ) -> Result<HashSet<String>, CacheError> {
        if content_hashes.is_empty() {
            return Ok(HashSet::new());
        }
        let conn = match self.connect_if_exists()? {
            Some(c) => c,
            None => return Ok(HashSet::new()),
        };
        ensure_schema(&conn)?;

        // Deduplicate while preserving order
        let mut seen = HashSet::new();
        let unique: Vec<&str> = content_hashes
            .iter()
            .copied()
            .filter(|h| seen.insert(*h))
            .collect();

        let mut cached = HashSet::new();
        for chunk in unique.chunks(900) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{}", i))
                .collect::<Vec<_>>()
                .join(", ");
            let tl_idx = chunk.len() + 1;
            let tt_idx = chunk.len() + 2;
            let oh_idx = chunk.len() + 3;
            let profile_idx = chunk.len() + 4;
            let sql = format!(
                "SELECT content_hash FROM segment_cache
                 WHERE content_hash IN ({placeholders})
                   AND target_lang = ?{tl_idx}
                   AND template_type = ?{tt_idx}
                   AND options_hash = ?{oh_idx}
                   AND profile_id = ?{profile_idx}"
            );
            let mut params: Vec<rusqlite::types::Value> = chunk
                .iter()
                .map(|h| rusqlite::types::Value::Text(h.to_string()))
                .collect();
            params.push(rusqlite::types::Value::Text(scope.target_lang.to_owned()));
            params.push(rusqlite::types::Value::Text(scope.template_type.to_owned()));
            params.push(rusqlite::types::Value::Text(scope.options_hash.to_owned()));
            params.push(rusqlite::types::Value::Text(scope.profile_id.to_owned()));

            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params), |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            cached.extend(rows);
        }
        Ok(cached)
    }

    pub fn store_segment_cache(
        &self,
        content_hash: &str,
        scope: SegmentCacheScope<'_>,
        translated_text: &str,
        created_at: &str,
    ) -> Result<(), CacheError> {
        let conn = self.connect_create()?;
        ensure_schema(&conn)?;
        conn.execute(
            "INSERT INTO segment_cache
                 (content_hash, target_lang, template_type, options_hash, profile_id, translated_text, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(content_hash, target_lang, template_type, options_hash, profile_id)
             DO UPDATE SET
                 translated_text = excluded.translated_text,
                 created_at = excluded.created_at",
            rusqlite::params![
                content_hash,
                scope.target_lang,
                scope.template_type,
                scope.options_hash,
                scope.profile_id,
                translated_text,
                created_at,
            ],
        )?;
        Ok(())
    }

    pub fn fetch_recent(&self, limit: Option<usize>) -> Result<Vec<TaskRecord>, CacheError> {
        let conn = match self.connect_if_exists()? {
            Some(c) => c,
            None => return Ok(Vec::new()),
        };
        ensure_schema(&conn)?;
        let sql = match limit {
            Some(n) => format!(
                "SELECT * FROM tasks ORDER BY finished_at DESC, id DESC LIMIT {}",
                n
            ),
            None => "SELECT * FROM tasks ORDER BY finished_at DESC, id DESC".to_owned(),
        };
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], record_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn fetch_recent_output(&self, position: usize) -> Result<Option<String>, CacheError> {
        if position < 1 {
            return Err(CacheError::InvalidArg(
                "position must be at least 1".to_owned(),
            ));
        }
        let conn = match self.connect_if_exists()? {
            Some(c) => c,
            None => return Ok(None),
        };
        ensure_schema(&conn)?;
        let result = conn.query_row(
            "SELECT output_text FROM tasks
             WHERE output_text IS NOT NULL
             ORDER BY finished_at DESC, id DESC
             LIMIT 1 OFFSET ?1",
            rusqlite::params![position as i64 - 1],
            |row| row.get::<_, Option<String>>(0),
        );
        match result {
            Ok(text) => Ok(text),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CacheError::Db(e)),
        }
    }

    pub fn count_translations(&self) -> Result<usize, CacheError> {
        let conn = match self.connect_if_exists()? {
            Some(c) => c,
            None => return Ok(0),
        };
        ensure_schema(&conn)?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE output_text IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn fetch_recent_translations(
        &self,
        limit: usize,
    ) -> Result<Vec<TranslationPreview>, CacheError> {
        let conn = match self.connect_if_exists()? {
            Some(c) => c,
            None => return Ok(Vec::new()),
        };
        ensure_schema(&conn)?;
        let mut stmt = conn.prepare(
            "SELECT id, finished_at, target_lang, template_type, output_chars, output_text
             FROM tasks
             WHERE output_text IS NOT NULL
             ORDER BY finished_at DESC, id DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let previews = rows
            .into_iter()
            .enumerate()
            .map(
                |(i, (id, finished_at, target_lang, template_type, output_chars, output_text))| {
                    TranslationPreview {
                        position: i + 1,
                        id,
                        finished_at,
                        target_lang,
                        template_type,
                        output_chars,
                        preview: output_text.as_deref().map(preview_text).unwrap_or_default(),
                    }
                },
            )
            .collect();
        Ok(previews)
    }

    /// Return performance statistics filtered by optional dimensions.
    pub fn stats(
        &self,
        target_lang: Option<&str>,
        template_type: Option<&str>,
        config_version: Option<i64>,
    ) -> Result<Option<PerformanceStats>, CacheError> {
        let conn = match self.connect_if_exists()? {
            Some(c) => c,
            None => return Ok(None),
        };
        ensure_schema(&conn)?;

        let mut conditions: Vec<&str> = vec!["tokens_per_second > 0", "segments > 0"];
        let mut params: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(tl) = target_lang {
            conditions.push("target_lang = ?");
            params.push(rusqlite::types::Value::Text(tl.to_owned()));
        }
        if let Some(tt) = template_type {
            conditions.push("template_type = ?");
            params.push(rusqlite::types::Value::Text(tt.to_owned()));
        }
        if let Some(cv) = config_version {
            conditions.push("config_version = ?");
            params.push(rusqlite::types::Value::Integer(cv));
        }

        // Re-index placeholders to ?1, ?2, ...
        let where_clause = build_where_clause(&conditions, &mut params);
        let sql = format!(
            "SELECT duration_seconds, input_tokens, output_tokens, segments, tokens_per_second
             FROM tasks
             WHERE {where_clause}
             ORDER BY tokens_per_second"
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<(f64, i64, i64, i64, f64)> = stmt
            .query_map(rusqlite::params_from_iter(params), |row| {
                Ok((
                    row.get::<_, f64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, f64>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(stats_from_rows(&rows))
    }

    /// Estimate translation duration.
    ///
    /// Returns `None` when there is insufficient history data — never returns an
    /// estimate with `seconds = 0` (that was the bug in issue #53).
    pub fn estimate(
        &self,
        segments: i64,
        concurrency: i64,
        target_lang: Option<&str>,
        template_type: Option<&str>,
        config_version: Option<i64>,
        min_samples: Option<usize>,
    ) -> Result<Option<DurationEstimate>, CacheError> {
        let min_samples = min_samples.unwrap_or(3);
        let mut versions_used: Vec<i64> = config_version.into_iter().collect();

        let stats = self.stats(target_lang, template_type, config_version)?;
        if let Some(ref s) = stats {
            if s.count >= min_samples && s.avg_tokens_per_second > 0.0 {
                return Ok(Some(build_estimate(
                    s,
                    segments,
                    concurrency,
                    versions_used,
                )));
            }
        }

        // Fallback: broaden by dropping config_version filter
        if config_version.is_some() {
            let broader = self.stats(target_lang, template_type, None)?;
            if let Some(ref s) = broader {
                if s.count >= min_samples && s.avg_tokens_per_second > 0.0 {
                    versions_used = self.distinct_versions()?;
                    return Ok(Some(build_estimate(
                        s,
                        segments,
                        concurrency,
                        versions_used,
                    )));
                }
            }
        }

        // Fallback: broaden by dropping lang/template filters
        if target_lang.is_some() || template_type.is_some() {
            let fallback = self.stats(None, None, config_version)?;
            if let Some(ref s) = fallback {
                if s.count >= min_samples && s.avg_tokens_per_second > 0.0 {
                    return Ok(Some(build_estimate(
                        s,
                        segments,
                        concurrency,
                        versions_used.clone(),
                    )));
                }
            }
            let global = self.stats(None, None, None)?;
            if let Some(ref s) = global {
                if s.count >= min_samples && s.avg_tokens_per_second > 0.0 {
                    versions_used = self.distinct_versions()?;
                    return Ok(Some(build_estimate(
                        s,
                        segments,
                        concurrency,
                        versions_used,
                    )));
                }
            }
        }

        // No usable history — return None (issue #53 fix: never return seconds=0)
        Ok(None)
    }

    pub fn clear(&self) -> Result<usize, CacheError> {
        let conn = match self.connect_if_exists()? {
            Some(c) => c,
            None => return Ok(0),
        };
        ensure_schema(&conn)?;
        conn.execute("DELETE FROM segment_cache", [])?;
        let deleted = conn.execute("DELETE FROM tasks", [])?;
        Ok(deleted)
    }

    fn connect_create(&self) -> Result<Connection, CacheError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&self.path)?;
        Ok(conn)
    }

    fn connect_if_exists(&self) -> Result<Option<Connection>, CacheError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(&self.path)?;
        Ok(Some(conn))
    }

    fn distinct_versions(&self) -> Result<Vec<i64>, CacheError> {
        let conn = match self.connect_if_exists()? {
            Some(c) => c,
            None => return Ok(Vec::new()),
        };
        ensure_schema(&conn)?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT config_version FROM tasks
             WHERE config_version IS NOT NULL
             ORDER BY config_version",
        )?;
        let versions = stmt
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(versions)
    }
}

fn ensure_schema(conn: &Connection) -> Result<(), CacheError> {
    conn.execute_batch(SCHEMA)?;
    migrate_tasks_columns(conn)?;
    migrate_segment_cache_columns(conn)?;
    Ok(())
}

fn migrate_tasks_columns(conn: &Connection) -> Result<(), CacheError> {
    let cols = table_column_names(conn, "tasks")?;
    if !cols.contains("output_text") {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN output_text TEXT")?;
    }
    if !cols.contains("input_hash") {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN input_hash TEXT")?;
    }
    if !cols.contains("config_version") {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN config_version INTEGER DEFAULT 1")?;
    }
    if !cols.contains("profile_id") {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN profile_id TEXT NOT NULL DEFAULT ''")?;
    }
    Ok(())
}

fn migrate_segment_cache_columns(conn: &Connection) -> Result<(), CacheError> {
    let cols = table_column_names(conn, "segment_cache")?;
    let mut rebuild_primary_key = false;
    if !cols.contains("options_hash") {
        conn.execute_batch(
            "ALTER TABLE segment_cache ADD COLUMN options_hash TEXT NOT NULL DEFAULT ''",
        )?;
        rebuild_primary_key = true;
    }
    if !cols.contains("profile_id") {
        conn.execute_batch(
            "ALTER TABLE segment_cache ADD COLUMN profile_id TEXT NOT NULL DEFAULT ''",
        )?;
        rebuild_primary_key = true;
    }
    if rebuild_primary_key {
        rebuild_segment_cache_pk(conn)?;
    }
    Ok(())
}

fn rebuild_segment_cache_pk(conn: &Connection) -> Result<(), CacheError> {
    conn.execute_batch(
        "CREATE TABLE segment_cache_new (
             content_hash TEXT NOT NULL,
             target_lang TEXT NOT NULL,
             template_type TEXT NOT NULL,
             options_hash TEXT NOT NULL DEFAULT '',
             profile_id TEXT NOT NULL DEFAULT '',
             translated_text TEXT NOT NULL,
             created_at TEXT NOT NULL,
             PRIMARY KEY (content_hash, target_lang, template_type, options_hash, profile_id)
         );
         INSERT OR REPLACE INTO segment_cache_new
             (content_hash, target_lang, template_type, options_hash, profile_id, translated_text, created_at)
         SELECT content_hash, target_lang, template_type,
                COALESCE(options_hash, ''), COALESCE(profile_id, ''), translated_text, created_at
         FROM segment_cache;
         DROP TABLE segment_cache;
         ALTER TABLE segment_cache_new RENAME TO segment_cache;",
    )?;
    Ok(())
}

fn table_column_names(conn: &Connection, table: &str) -> Result<HashSet<String>, CacheError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(cols)
}

fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    Ok(TaskRecord {
        id: row.get("id")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        duration_seconds: row.get("duration_seconds")?,
        input_tokens: row.get("input_tokens")?,
        output_tokens: row.get("output_tokens")?,
        segments: row.get("segments")?,
        concurrency: row.get("concurrency")?,
        source_lang: row.get("source_lang")?,
        target_lang: row.get("target_lang")?,
        template_type: row.get("template_type")?,
        model: row.get("model")?,
        profile_id: row
            .get::<_, Option<String>>("profile_id")?
            .unwrap_or_default(),
        tokens_per_second: row.get("tokens_per_second")?,
        input_chars: row.get("input_chars")?,
        output_chars: row.get("output_chars")?,
        output_text: row.get("output_text")?,
        input_hash: row.get("input_hash")?,
        config_version: row.get::<_, Option<i64>>("config_version")?.unwrap_or(1),
    })
}

fn preview_text(text: &str) -> String {
    let joined: String = text.split_whitespace().collect::<Vec<&str>>().join(" ");
    joined.chars().take(80).collect()
}

fn stats_from_rows(rows: &[(f64, i64, i64, i64, f64)]) -> Option<PerformanceStats> {
    if rows.is_empty() {
        return None;
    }
    // rows are sorted by tokens_per_second (ORDER BY in caller)
    let rates: Vec<f64> = rows.iter().map(|(_, _, _, _, tps)| *tps).collect();
    let total_output_tokens: i64 = rows.iter().map(|(_, _, out, _, _)| *out).sum();
    let total_segments: i64 = rows.iter().map(|(_, _, _, seg, _)| *seg).sum();
    let total_duration: f64 = rows.iter().map(|(dur, _, _, _, _)| *dur).sum();
    let total_input_tokens: i64 = rows.iter().map(|(_, inp, _, _, _)| *inp).sum();
    let n = rates.len();
    Some(PerformanceStats {
        count: n,
        avg_tokens_per_second: rates.iter().sum::<f64>() / n as f64,
        median_tokens_per_second: median(&rates),
        p5_tokens_per_second: percentile(&rates, 0.05),
        p95_tokens_per_second: percentile(&rates, 0.95),
        avg_output_tokens_per_segment: total_output_tokens as f64 / total_segments.max(1) as f64,
        total_duration_seconds: total_duration,
        total_input_tokens,
        total_output_tokens,
    })
}

fn build_estimate(
    stats: &PerformanceStats,
    segments: i64,
    concurrency: i64,
    versions_used: Vec<i64>,
) -> DurationEstimate {
    let effective_segments = segments.max(1) as f64;
    let effective_concurrency = concurrency.max(1).min(segments.max(1)) as f64;
    let estimated_output_tokens = stats.avg_output_tokens_per_segment * effective_segments;
    let seconds = estimated_output_tokens / stats.avg_tokens_per_second / effective_concurrency;
    DurationEstimate {
        stats: stats.clone(),
        seconds,
        concurrency,
        estimated_output_tokens,
        versions_used,
    }
}

/// Sorted ascending; already sorted by the SQL ORDER BY tokens_per_second.
fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    let raw = (p * n as f64).ceil() as usize;
    let idx = raw.saturating_sub(1).min(n - 1);
    sorted[idx]
}

/// Build a WHERE clause string with positional `?N` placeholders, rebasing
/// from simple `?` markers in `conditions`.
fn build_where_clause(conditions: &[&str], params: &mut Vec<rusqlite::types::Value>) -> String {
    // Replace bare `?` with `?N` placeholders
    let mut idx = 0usize;
    let parts: Vec<String> = conditions
        .iter()
        .map(|c| {
            if *c == "tokens_per_second > 0" || *c == "segments > 0" {
                c.to_string()
            } else {
                idx += 1;
                c.replacen('?', &format!("?{idx}"), 1)
            }
        })
        .collect();
    // params already in the right order; just drop unused mutable ref
    let _ = params;
    parts.join(" AND ")
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

    fn sample_record(tps: f64, segments: i64, output_tokens: i64) -> TaskRecord {
        TaskRecord {
            id: None,
            started_at: "2024-01-01T00:00:00Z".to_owned(),
            finished_at: "2024-01-01T00:00:10Z".to_owned(),
            duration_seconds: 10.0,
            input_tokens: 100,
            output_tokens,
            segments,
            concurrency: 1,
            source_lang: None,
            target_lang: "en".to_owned(),
            template_type: "default".to_owned(),
            model: None,
            profile_id: "hy_mt2_7b".to_owned(),
            tokens_per_second: tps,
            input_chars: 500,
            output_chars: 400,
            output_text: Some("translation output".to_owned()),
            input_hash: None,
            config_version: 1,
        }
    }

    #[test]
    fn test_insert_and_fetch_recent() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("history.db"));
        db.insert_task(&sample_record(50.0, 2, 200)).unwrap();
        let records = db.fetch_recent(Some(10)).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].target_lang, "en");
        assert_eq!(records[0].profile_id, "hy_mt2_7b");
        assert!((records[0].tokens_per_second - 50.0).abs() < 1e-9);
    }

    #[test]
    fn test_fetch_recent_no_db_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("absent.db"));
        let records = db.fetch_recent(Some(10)).unwrap();
        assert!(records.is_empty());
    }

    #[test]
    fn test_fetch_recent_output_by_position() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("history.db"));
        let mut r1 = sample_record(50.0, 1, 100);
        r1.finished_at = "2024-01-01T00:00:01Z".to_owned();
        r1.output_text = Some("first".to_owned());
        let mut r2 = sample_record(50.0, 1, 100);
        r2.finished_at = "2024-01-01T00:00:02Z".to_owned();
        r2.output_text = Some("second".to_owned());
        db.insert_task(&r1).unwrap();
        db.insert_task(&r2).unwrap();
        // position 1 = most recent
        assert_eq!(
            db.fetch_recent_output(1).unwrap().as_deref(),
            Some("second")
        );
        assert_eq!(db.fetch_recent_output(2).unwrap().as_deref(), Some("first"));
        assert!(db.fetch_recent_output(3).unwrap().is_none());
    }

    #[test]
    fn test_segment_cache_is_scoped_to_model_profile() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("history.db"));
        let seven_b = SegmentCacheScope {
            target_lang: "en",
            template_type: "default",
            options_hash: "",
            profile_id: "hy_mt2_7b",
        };
        let thirty_b = SegmentCacheScope {
            profile_id: "hy_mt2_30b_a3b",
            ..seven_b
        };
        db.store_segment_cache("hash1", seven_b, "translation", "2024-01-01T00:00:00Z")
            .unwrap();
        let found = db.find_segment_cached("hash1", seven_b).unwrap();
        assert_eq!(found.as_deref(), Some("translation"));
        let miss = db.find_segment_cached("hash1", thirty_b).unwrap();
        assert!(
            miss.is_none(),
            "a different profile must not reuse this cache entry"
        );
    }

    #[test]
    fn test_find_cached_segment_hashes() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("history.db"));
        let scope = SegmentCacheScope {
            target_lang: "en",
            template_type: "default",
            options_hash: "",
            profile_id: "hy_mt2_7b",
        };
        db.store_segment_cache("aaa", scope, "t1", "2024-01-01T00:00:00Z")
            .unwrap();
        db.store_segment_cache("bbb", scope, "t2", "2024-01-01T00:00:00Z")
            .unwrap();
        let found = db
            .find_cached_segment_hashes(&["aaa", "bbb", "ccc"], scope)
            .unwrap();
        assert!(found.contains("aaa"));
        assert!(found.contains("bbb"));
        assert!(!found.contains("ccc"));
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn test_stats_empty_returns_none() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("history.db"));
        let s = db.stats(None, None, None).unwrap();
        assert!(s.is_none());
    }

    #[test]
    fn test_stats_with_data() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("history.db"));
        db.insert_task(&sample_record(40.0, 2, 200)).unwrap();
        db.insert_task(&sample_record(60.0, 2, 200)).unwrap();
        let s = db.stats(None, None, None).unwrap().unwrap();
        assert_eq!(s.count, 2);
        assert!((s.avg_tokens_per_second - 50.0).abs() < 1e-9);
        // 200 out_tokens / 2 segments per record, 2 records → total 400/4 = 100.0
        assert!((s.avg_output_tokens_per_segment - 100.0).abs() < 1e-9);
    }

    #[test]
    fn test_estimate_empty_returns_none() {
        // Issue #53 fix: no history data → None, not DurationEstimate { seconds: 0.0 }
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("history.db"));
        let est = db.estimate(5, 2, None, None, None, None).unwrap();
        assert!(
            est.is_none(),
            "expected None when no history, got Some({:?})",
            est.map(|e| e.seconds)
        );
    }

    #[test]
    fn test_estimate_with_data() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("history.db"));
        for _ in 0..4 {
            db.insert_task(&sample_record(100.0, 5, 500)).unwrap();
        }
        let est = db.estimate(5, 1, None, None, None, None).unwrap();
        assert!(est.is_some());
        let est = est.unwrap();
        assert!(
            est.seconds > 0.0,
            "estimate must be positive, got {}",
            est.seconds
        );
        // 100 output_tokens/seg * 5 segs = 500 tokens / 100 tps / 1 concurrency = 5s
        assert!((est.seconds - 5.0).abs() < 0.5);
    }

    #[test]
    fn test_estimate_below_min_samples_returns_none() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("history.db"));
        // Only 2 records, min_samples=3 → None
        db.insert_task(&sample_record(100.0, 5, 500)).unwrap();
        db.insert_task(&sample_record(100.0, 5, 500)).unwrap();
        let est = db.estimate(5, 1, None, None, None, Some(3)).unwrap();
        assert!(est.is_none());
    }

    #[test]
    fn test_estimate_fallback_below_min_samples_returns_none() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("history.db"));
        db.insert_task(&sample_record(100.0, 1, 1)).unwrap();

        let broadened_config = db
            .estimate(1, 1, Some("en"), Some("default"), Some(2), None)
            .unwrap();
        assert!(
            broadened_config.is_none(),
            "config-version fallback must not estimate from one sample"
        );

        let global = db
            .estimate(1, 1, Some("zh"), Some("default"), Some(2), None)
            .unwrap();
        assert!(
            global.is_none(),
            "global fallback must not estimate from one sample"
        );
    }

    #[test]
    fn test_clear_removes_all_records() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("history.db"));
        db.insert_task(&sample_record(50.0, 2, 100)).unwrap();
        db.insert_task(&sample_record(60.0, 3, 150)).unwrap();
        let deleted = db.clear().unwrap();
        assert_eq!(deleted, 2);
        assert!(db.fetch_recent(None).unwrap().is_empty());
    }

    #[test]
    fn test_count_translations() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDB::new(tmp.path().join("history.db"));
        db.insert_task(&sample_record(50.0, 1, 100)).unwrap();
        let mut no_output = sample_record(50.0, 1, 100);
        no_output.output_text = None;
        db.insert_task(&no_output).unwrap();
        assert_eq!(db.count_translations().unwrap(), 1);
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0.0), "0s");
        assert_eq!(format_duration(-1.0), "0s");
        assert_eq!(format_duration(45.0), "45s");
        assert_eq!(format_duration(65.0), "1m05s");
        assert_eq!(format_duration(3665.0), "1h01m05s");
        assert_eq!(format_duration(f64::INFINITY), "0s");
    }
}
