from __future__ import annotations

import os
import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from click.testing import CliRunner

from hymt.cli import main
from hymt.history import HistoryDB, TaskRecord


class RecallCommandTests(unittest.TestCase):
    def test_recall_prints_latest_output_exactly(self) -> None:
        with temporary_home() as home:
            insert_output("older output")
            insert_output("latest output\nwith newline")

            result = CliRunner().invoke(main, ["recall"], env={"HOME": home})

        self.assertEqual(result.exit_code, 0, result.output)
        self.assertEqual(result.output, "latest output\nwith newline")

    def test_recall_n_prints_nth_most_recent_output(self) -> None:
        with temporary_home() as home:
            insert_output("first")
            insert_output("second")

            result = CliRunner().invoke(main, ["recall", "-n", "2"], env={"HOME": home})

        self.assertEqual(result.exit_code, 0, result.output)
        self.assertEqual(result.output, "first")

    def test_recall_reports_empty_history_on_stderr(self) -> None:
        with temporary_home() as home:
            result = CliRunner().invoke(main, ["recall"], env={"HOME": home})

        self.assertEqual(result.exit_code, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "No translation history.\n")

    def test_recall_reports_out_of_range_count_on_stderr(self) -> None:
        with temporary_home() as home:
            insert_output("only output")

            result = CliRunner().invoke(main, ["recall", "-n", "2"], env={"HOME": home})

        self.assertEqual(result.exit_code, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "Only 1 translations in history.\n")

    def test_recall_list_shows_recent_preview(self) -> None:
        long_output = f"{'x' * 90}\nsecond line"
        with temporary_home() as home:
            insert_output(long_output)

            result = CliRunner().invoke(main, ["recall", "--list"], env={"HOME": home})

        self.assertEqual(result.exit_code, 0, result.output)
        self.assertIn("Preview", result.output)
        self.assertIn("x" * 80, result.output)
        self.assertNotIn("second line", result.output)


class HistoryMigrationTests(unittest.TestCase):
    def test_insert_task_migrates_existing_tasks_table(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            path = Path(tmpdir) / "history.db"
            connection = sqlite3.connect(path)
            try:
                connection.execute(OLD_TASKS_SCHEMA)
                connection.commit()
            finally:
                connection.close()

            db = HistoryDB(path)
            db.insert_task(task_record("migrated output"))

            self.assertEqual(db.fetch_recent_output(), "migrated output")

    def test_segment_cache_returns_matching_output(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            path = Path(tmpdir) / "history.db"
            db = HistoryDB(path)
            content_hash = "segment-hash"

            db.store_segment_cache(
                content_hash,
                "en",
                "default",
                "cached output",
                "2026-05-23T00:00:00+00:00",
            )
            db.store_segment_cache(
                content_hash,
                "ja",
                "default",
                "wrong language",
                "2026-05-23T00:00:00+00:00",
            )
            db.store_segment_cache(
                content_hash,
                "en",
                "style",
                "wrong template",
                "2026-05-23T00:00:00+00:00",
            )

            self.assertEqual(
                db.find_segment_cached(
                    content_hash, target_lang="en", template_type="default"
                ),
                "cached output",
            )
            self.assertIsNone(
                db.find_segment_cached(
                    "missing", target_lang="en", template_type="default"
                )
            )


class temporary_home:
    def __enter__(self) -> str:
        self._tmpdir = tempfile.TemporaryDirectory()
        self._patch = patch.dict(os.environ, {"HOME": self._tmpdir.name})
        self._patch.__enter__()
        return self._tmpdir.name

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        self._patch.__exit__(exc_type, exc, traceback)
        self._tmpdir.cleanup()


def insert_output(output_text: str) -> None:
    db = HistoryDB()
    db.insert_task(task_record(output_text))


def task_record(
    output_text: str,
    *,
    input_hash: str | None = None,
    target_lang: str = "en",
    template_type: str = "default",
) -> TaskRecord:
    return TaskRecord(
        started_at="2026-05-23T00:00:00+00:00",
        finished_at=f"2026-05-23T00:00:{len(output_text):02d}+00:00",
        duration_seconds=1.0,
        input_tokens=1,
        output_tokens=1,
        segments=1,
        concurrency=1,
        source_lang=None,
        target_lang=target_lang,
        template_type=template_type,
        model=None,
        tokens_per_second=1.0,
        input_chars=4,
        output_chars=len(output_text),
        output_text=output_text,
        input_hash=input_hash,
    )


OLD_TASKS_SCHEMA = """
CREATE TABLE tasks (
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


if __name__ == "__main__":
    unittest.main()
