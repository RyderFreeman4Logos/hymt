from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass
from math import ceil, isfinite
from pathlib import Path
from statistics import median
import sqlite3


__all__ = [
    "DurationEstimate",
    "HistoryDB",
    "PerformanceStats",
    "TaskRecord",
    "TranslationPreview",
    "estimate_duration_seconds",
    "format_duration",
    "history_path",
]


SCHEMA = """
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
    input_hash TEXT
);

CREATE TABLE IF NOT EXISTS segment_cache (
    content_hash TEXT NOT NULL,
    target_lang TEXT NOT NULL,
    template_type TEXT NOT NULL,
    options_hash TEXT NOT NULL DEFAULT '',
    translated_text TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (content_hash, target_lang, template_type, options_hash)
);
"""


@dataclass(frozen=True)
class TaskRecord:
    started_at: str
    finished_at: str
    duration_seconds: float
    input_tokens: int
    output_tokens: int
    segments: int
    concurrency: int
    source_lang: str | None
    target_lang: str
    template_type: str
    model: str | None
    tokens_per_second: float
    input_chars: int
    output_chars: int
    output_text: str | None = None
    input_hash: str | None = None
    config_version: int = 1
    id: int | None = None


@dataclass(frozen=True)
class TranslationPreview:
    position: int
    id: int
    finished_at: str
    target_lang: str
    template_type: str
    output_chars: int
    preview: str


@dataclass(frozen=True)
class PerformanceStats:
    count: int
    avg_tokens_per_second: float
    median_tokens_per_second: float
    p5_tokens_per_second: float
    p95_tokens_per_second: float
    avg_output_tokens_per_segment: float
    total_duration_seconds: float
    total_input_tokens: int
    total_output_tokens: int


@dataclass(frozen=True)
class DurationEstimate:
    stats: PerformanceStats
    seconds: float
    concurrency: int
    estimated_output_tokens: float
    versions_used: tuple[int, ...] = ()


def history_path() -> Path:
    return Path.home() / ".local" / "share" / "hymt" / "history.db"


def estimate_duration_seconds(
    stats: PerformanceStats, segments: int, concurrency: int
) -> float:
    effective_segments = max(1, segments)
    effective_concurrency = max(1, min(max(1, concurrency), effective_segments))
    estimated_output_tokens = stats.avg_output_tokens_per_segment * effective_segments
    if stats.avg_tokens_per_second <= 0:
        return 0.0
    return estimated_output_tokens / stats.avg_tokens_per_second / effective_concurrency


def format_duration(seconds: float) -> str:
    if not isfinite(seconds) or seconds <= 0:
        return "0s"
    total_seconds = int(round(seconds))
    hours, remainder = divmod(total_seconds, 3600)
    minutes, remaining_seconds = divmod(remainder, 60)
    if hours:
        return f"{hours}h{minutes:02d}m{remaining_seconds:02d}s"
    if minutes:
        return f"{minutes}m{remaining_seconds:02d}s"
    return f"{remaining_seconds}s"


class HistoryDB:
    _schema_verified_paths: set[tuple[Path, int]] = set()
    _schema_version = 3

    def __init__(self, path: Path | str | None = None) -> None:
        self._path = Path(path).expanduser() if path is not None else history_path()

    @property
    def path(self) -> Path:
        return self._path

    def insert_task(self, record: TaskRecord) -> None:
        connection = self._connect(create=True)
        try:
            self._ensure_schema(connection)
            connection.execute(
                """
                INSERT INTO tasks (
                    started_at,
                    finished_at,
                    duration_seconds,
                    input_tokens,
                    output_tokens,
                    segments,
                    concurrency,
                    source_lang,
                    target_lang,
                    template_type,
                    model,
                    tokens_per_second,
                    input_chars,
                    output_chars,
                    output_text,
                    input_hash,
                    config_version
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
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
                ),
            )
            connection.commit()
        finally:
            connection.close()

    def find_segment_cached(
        self,
        content_hash: str,
        target_lang: str,
        template_type: str,
        options_hash: str = "",
    ) -> str | None:
        connection = self._connect_if_exists()
        if connection is None:
            return None
        try:
            self._ensure_schema(connection)
            row = connection.execute(
                """
                SELECT translated_text
                FROM segment_cache
                WHERE content_hash = ?
                  AND target_lang = ?
                  AND template_type = ?
                  AND options_hash = ?
                LIMIT 1
                """,
                (content_hash, target_lang, template_type, options_hash),
            ).fetchone()
            if row is None:
                return None
            return str(row["translated_text"])
        finally:
            connection.close()

    def find_cached_segment_hashes(
        self,
        content_hashes: Iterable[str],
        target_lang: str,
        template_type: str,
        options_hash: str = "",
    ) -> set[str]:
        unique_hashes = tuple(dict.fromkeys(content_hashes))
        if not unique_hashes:
            return set()
        connection = self._connect_if_exists()
        if connection is None:
            return set()
        try:
            self._ensure_schema(connection)
            cached: set[str] = set()
            for chunk in _chunks(unique_hashes, 900):
                placeholders = ",".join("?" for _ in chunk)
                rows = connection.execute(
                    f"""
                    SELECT content_hash
                    FROM segment_cache
                    WHERE content_hash IN ({placeholders})
                      AND target_lang = ?
                      AND template_type = ?
                      AND options_hash = ?
                    """,
                    (*chunk, target_lang, template_type, options_hash),
                ).fetchall()
                cached.update(str(row["content_hash"]) for row in rows)
            return cached
        finally:
            connection.close()

    def store_segment_cache(
        self,
        content_hash: str,
        target_lang: str,
        template_type: str,
        translated_text: str,
        created_at: str,
        options_hash: str = "",
    ) -> None:
        connection = self._connect(create=True)
        try:
            self._ensure_schema(connection)
            connection.execute(
                """
                INSERT INTO segment_cache (
                    content_hash,
                    target_lang,
                    template_type,
                    options_hash,
                    translated_text,
                    created_at
                )
                VALUES (?, ?, ?, ?, ?, ?)
                ON CONFLICT(content_hash, target_lang, template_type, options_hash)
                DO UPDATE SET
                    translated_text = excluded.translated_text,
                    created_at = excluded.created_at
                """,
                (
                    content_hash,
                    target_lang,
                    template_type,
                    options_hash,
                    translated_text,
                    created_at,
                ),
            )
            connection.commit()
        finally:
            connection.close()

    def fetch_recent(self, limit: int | None = 10) -> list[TaskRecord]:
        connection = self._connect_if_exists()
        if connection is None:
            return []
        try:
            self._ensure_schema(connection)
            sql = "SELECT * FROM tasks ORDER BY finished_at DESC, id DESC"
            parameters: tuple[int, ...] = ()
            if limit is not None:
                sql = f"{sql} LIMIT ?"
                parameters = (limit,)
            rows = connection.execute(sql, parameters).fetchall()
            return [_record_from_row(row) for row in rows]
        finally:
            connection.close()

    def fetch_recent_output(self, position: int = 1) -> str | None:
        if position < 1:
            raise ValueError("position must be at least 1")
        connection = self._connect_if_exists()
        if connection is None:
            return None
        try:
            self._ensure_schema(connection)
            row = connection.execute(
                """
                SELECT output_text
                FROM tasks
                WHERE output_text IS NOT NULL
                ORDER BY finished_at DESC, id DESC
                LIMIT 1 OFFSET ?
                """,
                (position - 1,),
            ).fetchone()
            if row is None:
                return None
            return str(row["output_text"])
        finally:
            connection.close()

    def count_translations(self) -> int:
        connection = self._connect_if_exists()
        if connection is None:
            return 0
        try:
            self._ensure_schema(connection)
            row = connection.execute(
                "SELECT COUNT(*) AS count FROM tasks WHERE output_text IS NOT NULL"
            ).fetchone()
            return int(row["count"]) if row is not None else 0
        finally:
            connection.close()

    def fetch_recent_translations(self, limit: int = 10) -> list[TranslationPreview]:
        connection = self._connect_if_exists()
        if connection is None:
            return []
        try:
            self._ensure_schema(connection)
            rows = connection.execute(
                """
                SELECT
                    id,
                    finished_at,
                    target_lang,
                    template_type,
                    output_chars,
                    output_text
                FROM tasks
                WHERE output_text IS NOT NULL
                ORDER BY finished_at DESC, id DESC
                LIMIT ?
                """,
                (max(0, limit),),
            ).fetchall()
            return [
                TranslationPreview(
                    position=index,
                    id=int(row["id"]),
                    finished_at=str(row["finished_at"]),
                    target_lang=str(row["target_lang"]),
                    template_type=str(row["template_type"]),
                    output_chars=int(row["output_chars"]),
                    preview=_preview_text(str(row["output_text"])),
                )
                for index, row in enumerate(rows, start=1)
            ]
        finally:
            connection.close()

    def stats(
        self,
        target_lang: str | None = None,
        template_type: str | None = None,
        config_version: int | None = None,
    ) -> PerformanceStats | None:
        connection = self._connect_if_exists()
        if connection is None:
            return None
        try:
            self._ensure_schema(connection)
            where, parameters = _stats_filters(
                target_lang, template_type, config_version
            )
            rows = connection.execute(
                f"""
                SELECT
                    duration_seconds,
                    input_tokens,
                    output_tokens,
                    segments,
                    tokens_per_second,
                    config_version
                FROM tasks
                {where}
                ORDER BY tokens_per_second
                """,
                parameters,
            ).fetchall()
            return _stats_from_rows(rows)
        finally:
            connection.close()

    def estimate(
        self,
        segments: int,
        concurrency: int,
        target_lang: str | None = None,
        template_type: str | None = None,
        config_version: int | None = None,
        min_samples: int = 3,
    ) -> DurationEstimate | None:
        stats = self.stats(target_lang, template_type, config_version)
        versions_used: tuple[int, ...] = ()
        if config_version is not None:
            versions_used = (config_version,)
        if stats is not None and stats.count >= min_samples:
            return _build_estimate(stats, segments, concurrency, versions_used)
        if config_version is not None:
            broader = self.stats(target_lang, template_type)
            if broader is not None:
                all_versions = self._distinct_versions()
                versions_used = tuple(all_versions)
                return _build_estimate(broader, segments, concurrency, versions_used)
        if target_lang is not None or template_type is not None:
            fallback = self.stats(config_version=config_version)
            if fallback is not None and fallback.count >= min_samples:
                return _build_estimate(fallback, segments, concurrency, versions_used)
            global_fallback = self.stats()
            if global_fallback is not None:
                all_versions = self._distinct_versions()
                return _build_estimate(
                    global_fallback, segments, concurrency, tuple(all_versions)
                )
        return None

    def _distinct_versions(self) -> list[int]:
        connection = self._connect_if_exists()
        if connection is None:
            return []
        try:
            self._ensure_schema(connection)
            rows = connection.execute(
                "SELECT DISTINCT config_version FROM tasks WHERE config_version IS NOT NULL ORDER BY config_version"
            ).fetchall()
            return [int(row["config_version"]) for row in rows]
        finally:
            connection.close()

    def clear(self) -> int:
        connection = self._connect_if_exists()
        if connection is None:
            return 0
        try:
            self._ensure_schema(connection)
            connection.execute("DELETE FROM segment_cache")
            cursor = connection.execute("DELETE FROM tasks")
            connection.commit()
            return cursor.rowcount if cursor.rowcount >= 0 else 0
        finally:
            connection.close()

    def _connect(self, create: bool) -> sqlite3.Connection:
        if create:
            if not self._path.exists():
                self._schema_verified_paths.discard(self._schema_cache_key())
            self._path.parent.mkdir(parents=True, exist_ok=True)
        connection = sqlite3.connect(str(self._path))
        connection.row_factory = sqlite3.Row
        return connection

    def _connect_if_exists(self) -> sqlite3.Connection | None:
        if not self._path.exists():
            return None
        return self._connect(create=False)

    def _ensure_schema(self, connection: sqlite3.Connection) -> None:
        cache_key = self._schema_cache_key()
        if cache_key in self._schema_verified_paths:
            return
        connection.executescript(SCHEMA)
        columns = {
            str(row["name"])
            for row in connection.execute("PRAGMA table_info(tasks)").fetchall()
        }
        if "output_text" not in columns:
            connection.execute("ALTER TABLE tasks ADD COLUMN output_text TEXT")
        if "input_hash" not in columns:
            connection.execute("ALTER TABLE tasks ADD COLUMN input_hash TEXT")
        if "config_version" not in columns:
            connection.execute(
                "ALTER TABLE tasks ADD COLUMN config_version INTEGER DEFAULT 1"
            )
        segment_columns = {
            str(row["name"]): int(row["pk"])
            for row in connection.execute("PRAGMA table_info(segment_cache)").fetchall()
        }
        if "options_hash" not in segment_columns:
            connection.execute(
                "ALTER TABLE segment_cache ADD COLUMN options_hash TEXT NOT NULL DEFAULT ''"
            )
            segment_columns["options_hash"] = 0
        if segment_columns.get("options_hash", 0) == 0:
            _rebuild_segment_cache_primary_key(connection)
        connection.commit()
        self._schema_verified_paths.add(cache_key)

    def _schema_cache_key(self) -> tuple[Path, int]:
        return (self._path.resolve(strict=False), self._schema_version)


def _build_estimate(
    stats: PerformanceStats,
    segments: int,
    concurrency: int,
    versions_used: tuple[int, ...],
) -> DurationEstimate:
    effective_segments = max(1, segments)
    estimated_output_tokens = stats.avg_output_tokens_per_segment * effective_segments
    return DurationEstimate(
        stats=stats,
        seconds=estimate_duration_seconds(stats, segments, concurrency),
        concurrency=concurrency,
        estimated_output_tokens=estimated_output_tokens,
        versions_used=versions_used,
    )


def _rebuild_segment_cache_primary_key(connection: sqlite3.Connection) -> None:
    connection.execute(
        """
        CREATE TABLE segment_cache_new (
            content_hash TEXT NOT NULL,
            target_lang TEXT NOT NULL,
            template_type TEXT NOT NULL,
            options_hash TEXT NOT NULL DEFAULT '',
            translated_text TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (content_hash, target_lang, template_type, options_hash)
        )
        """
    )
    connection.execute(
        """
        INSERT OR REPLACE INTO segment_cache_new (
            content_hash,
            target_lang,
            template_type,
            options_hash,
            translated_text,
            created_at
        )
        SELECT
            content_hash,
            target_lang,
            template_type,
            COALESCE(options_hash, ''),
            translated_text,
            created_at
        FROM segment_cache
        """
    )
    connection.execute("DROP TABLE segment_cache")
    connection.execute("ALTER TABLE segment_cache_new RENAME TO segment_cache")


def _record_from_row(row: sqlite3.Row) -> TaskRecord:
    cv = row["config_version"] if "config_version" in row.keys() else 1
    input_hash = row["input_hash"] if "input_hash" in row.keys() else None
    return TaskRecord(
        id=int(row["id"]),
        started_at=str(row["started_at"]),
        finished_at=str(row["finished_at"]),
        duration_seconds=float(row["duration_seconds"]),
        input_tokens=int(row["input_tokens"]),
        output_tokens=int(row["output_tokens"]),
        segments=int(row["segments"]),
        concurrency=int(row["concurrency"]),
        source_lang=row["source_lang"]
        if row["source_lang"] is None
        else str(row["source_lang"]),
        target_lang=str(row["target_lang"]),
        template_type=str(row["template_type"]),
        model=row["model"] if row["model"] is None else str(row["model"]),
        tokens_per_second=float(row["tokens_per_second"]),
        input_chars=int(row["input_chars"]),
        output_chars=int(row["output_chars"]),
        output_text=row["output_text"]
        if row["output_text"] is None
        else str(row["output_text"]),
        input_hash=input_hash if input_hash is None else str(input_hash),
        config_version=int(cv) if cv is not None else 1,
    )


def _preview_text(text: str, limit: int = 80) -> str:
    preview = " ".join(text.split())
    return preview[:limit]


def _stats_filters(
    target_lang: str | None,
    template_type: str | None,
    config_version: int | None = None,
) -> tuple[str, tuple[str | int, ...]]:
    filters: list[str] = ["tokens_per_second > 0", "segments > 0"]
    parameters: list[str | int] = []
    if target_lang is not None:
        filters.append("target_lang = ?")
        parameters.append(target_lang)
    if template_type is not None:
        filters.append("template_type = ?")
        parameters.append(template_type)
    if config_version is not None:
        filters.append("config_version = ?")
        parameters.append(config_version)
    return f"WHERE {' AND '.join(filters)}", tuple(parameters)


def _stats_from_rows(rows: list[sqlite3.Row]) -> PerformanceStats | None:
    if not rows:
        return None
    rates = [float(row["tokens_per_second"]) for row in rows]
    total_output_tokens = sum(int(row["output_tokens"]) for row in rows)
    total_segments = sum(int(row["segments"]) for row in rows)
    total_duration = sum(float(row["duration_seconds"]) for row in rows)
    total_input_tokens = sum(int(row["input_tokens"]) for row in rows)
    return PerformanceStats(
        count=len(rows),
        avg_tokens_per_second=sum(rates) / len(rates),
        median_tokens_per_second=float(median(rates)),
        p5_tokens_per_second=_percentile(rates, 0.05),
        p95_tokens_per_second=_percentile(rates, 0.95),
        avg_output_tokens_per_segment=total_output_tokens / max(1, total_segments),
        total_duration_seconds=total_duration,
        total_input_tokens=total_input_tokens,
        total_output_tokens=total_output_tokens,
    )


def _percentile(sorted_values: list[float], percentile: float) -> float:
    if not sorted_values:
        return 0.0
    index = min(
        len(sorted_values) - 1, max(0, ceil(percentile * len(sorted_values)) - 1)
    )
    return sorted_values[index]


def _chunks(values: tuple[str, ...], size: int) -> list[tuple[str, ...]]:
    return [values[index : index + size] for index in range(0, len(values), size)]
