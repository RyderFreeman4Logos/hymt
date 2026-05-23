from __future__ import annotations

import asyncio
import io
import os
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import AsyncMock, patch

from click.testing import CliRunner

from hymt.batch import build_batch_plan, run_batch_translation, show_batch_preview
from hymt.cli import main
from hymt.history import HistoryDB
from hymt.templates import TemplateType
from hymt.translate import _segment_cache_hash


class BatchPlanTests(unittest.TestCase):
    def test_build_batch_plan_scans_filters_and_reports_cache_status(self) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / "full.md").write_text("cached", encoding="utf-8")
            (root / "partial.md").write_text("cached|fresh", encoding="utf-8")
            (root / "target.md").write_text("中文段落。", encoding="utf-8")
            nested = root / "nested"
            nested.mkdir()
            (nested / "readme.txt").write_text("fresh", encoding="utf-8")
            if hasattr(os, "symlink"):
                os.symlink(nested, root / "linked")

            HistoryDB().store_segment_cache(
                _segment_cache_hash("cached"),
                "zh",
                TemplateType.DEFAULT.value,
                "缓存",
                "2026-05-23T00:00:00+00:00",
            )

            with patched_batch_dependencies():
                plan = build_batch_plan(
                    root,
                    root / "out",
                    "zh",
                    fake_config(),
                    TemplateType.DEFAULT,
                    {},
                )

            self.assertEqual(len(plan.skipped), 1)
            self.assertEqual(plan.skipped[0].relative_path, Path("target.md"))
            statuses = {
                file.relative_path.name: file.cache_status for file in plan.files
            }
            self.assertEqual(statuses["full.md"], "full")
            self.assertEqual(statuses["partial.md"], "partial")
            self.assertIn("readme.txt", statuses)

            partial = next(
                file for file in plan.files if file.relative_path.name == "partial.md"
            )
            self.assertEqual(partial.cached_segments, 1)
            self.assertEqual(partial.segment_count, 2)
            self.assertEqual(partial.output_path, root / "out" / "partial.zh.md")

            preview = io.StringIO()
            show_batch_preview(plan, preview)
            text = preview.getvalue()
            self.assertIn("cache=full", text)
            self.assertIn("cache=partial", text)
            self.assertIn("Total estimated time:", text)

    def test_run_batch_translation_writes_outputs_even_when_fully_cached(self) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / "a.md").write_text("cached", encoding="utf-8")
            (root / "b.md").write_text("fresh", encoding="utf-8")
            output_dir = root / "translated"

            HistoryDB().store_segment_cache(
                _segment_cache_hash("cached"),
                "zh",
                TemplateType.DEFAULT.value,
                "缓存",
                "2026-05-23T00:00:00+00:00",
            )

            with patched_batch_dependencies():
                plan = build_batch_plan(
                    root,
                    output_dir,
                    "zh",
                    fake_config(),
                    TemplateType.DEFAULT,
                    {},
                )

            translate = AsyncMock(side_effect=["缓存", "新译文"])
            with patch("hymt.batch.translate_text", translate):
                asyncio.run(
                    run_batch_translation(
                        plan,
                        "zh",
                        fake_config(),
                        TemplateType.DEFAULT,
                        progress_stream=io.StringIO(),
                    )
                )

            self.assertEqual(
                (output_dir / "a.zh.md").read_text(encoding="utf-8"), "缓存"
            )
            self.assertEqual(
                (output_dir / "b.zh.md").read_text(encoding="utf-8"), "新译文"
            )
            self.assertEqual(translate.await_count, 2)


class BatchCliTests(unittest.TestCase):
    def test_batch_without_write_is_dry_run(self) -> None:
        with temporary_home() as home, tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / "input.md").write_text("English.", encoding="utf-8")

            runner = CliRunner()
            with (
                patched_batch_dependencies(),
                patch("hymt.cli.HotConfig", return_value=fake_config()),
            ):
                result = runner.invoke(
                    main,
                    ["batch", "-t", "zh", str(root)],
                    env={"HOME": home},
                )

            self.assertEqual(result.exit_code, 0, result.output)
            self.assertIn("Dry run: no files written.", result.output)
            self.assertFalse((root / "input.zh.md").exists())


class patched_batch_dependencies:
    def __enter__(self) -> None:
        self._patches = [
            patch("hymt.language._load_langdetect", return_value=FakeDetector()),
            patch("hymt.translate.create_segmenter", return_value=FakeSegmenter()),
        ]
        for item in self._patches:
            item.__enter__()
        return None

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        for item in reversed(self._patches):
            item.__exit__(exc_type, exc, traceback)


class FakeDetector:
    def detect(self, text: str) -> str:
        if any("\u4e00" <= char <= "\u9fff" for char in text):
            return "zh"
        return "en"


class FakeSegmenter:
    def count_tokens(self, text: str) -> int:
        return len(text)

    def segment(self, text: str, max_tokens: int) -> list[str]:
        return [part for part in text.split("|") if part]


class temporary_home:
    def __enter__(self) -> str:
        self._tmpdir = tempfile.TemporaryDirectory()
        self._old_home = os.environ.get("HOME")
        os.environ["HOME"] = self._tmpdir.name
        return self._tmpdir.name

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        if self._old_home is None:
            os.environ.pop("HOME", None)
        else:
            os.environ["HOME"] = self._old_home
        self._tmpdir.cleanup()


def fake_config() -> SimpleNamespace:
    return SimpleNamespace(
        context_window=4096,
        max_output_tokens=128,
        concurrency=1,
        config_version=1,
        model="test-model",
        timing_divergence_threshold=2.0,
    )


if __name__ == "__main__":
    unittest.main()
