from __future__ import annotations

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
    output_chars INTEGER NOT NULL
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
    id: int | None = None


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


def history_path() -> Path:
    return Path.home() / ".local" / "share" / "hymt" / "history.db"


def estimate_duration_seconds(stats: PerformanceStats, segments: int, concurrency: int) -> float:
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
                    output_chars
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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

    def stats(
        self,
        target_lang: str | None = None,
        template_type: str | None = None,
    ) -> PerformanceStats | None:
        connection = self._connect_if_exists()
        if connection is None:
            return None
        try:
            self._ensure_schema(connection)
            where, parameters = _stats_filters(target_lang, template_type)
            rows = connection.execute(
                f"""
                SELECT
                    duration_seconds,
                    input_tokens,
                    output_tokens,
                    segments,
                    tokens_per_second
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
    ) -> DurationEstimate | None:
        stats = self.stats(target_lang, template_type)
        if stats is None and (target_lang is not None or template_type is not None):
            stats = self.stats()
        if stats is None:
            return None
        effective_segments = max(1, segments)
        estimated_output_tokens = stats.avg_output_tokens_per_segment * effective_segments
        return DurationEstimate(
            stats=stats,
            seconds=estimate_duration_seconds(stats, segments, concurrency),
            concurrency=concurrency,
            estimated_output_tokens=estimated_output_tokens,
        )

    def clear(self) -> int:
        connection = self._connect_if_exists()
        if connection is None:
            return 0
        try:
            self._ensure_schema(connection)
            cursor = connection.execute("DELETE FROM tasks")
            connection.commit()
            return cursor.rowcount if cursor.rowcount >= 0 else 0
        finally:
            connection.close()

    def _connect(self, create: bool) -> sqlite3.Connection:
        if create:
            self._path.parent.mkdir(parents=True, exist_ok=True)
        connection = sqlite3.connect(str(self._path))
        connection.row_factory = sqlite3.Row
        return connection

    def _connect_if_exists(self) -> sqlite3.Connection | None:
        if not self._path.exists():
            return None
        return self._connect(create=False)

    def _ensure_schema(self, connection: sqlite3.Connection) -> None:
        connection.execute(SCHEMA)


def _record_from_row(row: sqlite3.Row) -> TaskRecord:
    return TaskRecord(
        id=int(row["id"]),
        started_at=str(row["started_at"]),
        finished_at=str(row["finished_at"]),
        duration_seconds=float(row["duration_seconds"]),
        input_tokens=int(row["input_tokens"]),
        output_tokens=int(row["output_tokens"]),
        segments=int(row["segments"]),
        concurrency=int(row["concurrency"]),
        source_lang=row["source_lang"] if row["source_lang"] is None else str(row["source_lang"]),
        target_lang=str(row["target_lang"]),
        template_type=str(row["template_type"]),
        model=row["model"] if row["model"] is None else str(row["model"]),
        tokens_per_second=float(row["tokens_per_second"]),
        input_chars=int(row["input_chars"]),
        output_chars=int(row["output_chars"]),
    )


def _stats_filters(
    target_lang: str | None,
    template_type: str | None,
) -> tuple[str, tuple[str, ...]]:
    filters: list[str] = ["tokens_per_second > 0", "segments > 0"]
    parameters: list[str] = []
    if target_lang is not None:
        filters.append("target_lang = ?")
        parameters.append(target_lang)
    if template_type is not None:
        filters.append("template_type = ?")
        parameters.append(template_type)
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
    index = min(len(sorted_values) - 1, max(0, ceil(percentile * len(sorted_values)) - 1))
    return sorted_values[index]
