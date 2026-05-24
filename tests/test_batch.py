from __future__ import annotations

import asyncio
from contextlib import redirect_stderr
import io
import os
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import AsyncMock, patch

from click.testing import CliRunner

from hymt.batch import (
    _output_path,
    build_batch_plan,
    run_batch_translation,
    show_batch_preview,
)
from hymt.cli import main
from hymt.history import HistoryDB
from hymt.templates import TemplateType
from hymt.translate import _segment_cache_hash


class BatchPlanTests(unittest.TestCase):
    def test_build_batch_plan_scans_top_level_by_default(self) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / "top.md").write_text("fresh", encoding="utf-8")
            nested = root / "nested"
            nested.mkdir()
            (nested / "deep.txt").write_text("fresh", encoding="utf-8")

            with patched_batch_dependencies():
                plan = build_batch_plan(
                    root,
                    root / "out",
                    "zh",
                    fake_config(),
                    TemplateType.DEFAULT,
                    {},
                )

            self.assertEqual(
                [file.relative_path for file in plan.files],
                [Path("top.md")],
            )

    def test_build_batch_plan_reports_planning_progress_when_requested(self) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / "alpha.md").write_text("fresh", encoding="utf-8")
            nested = root / "nested"
            nested.mkdir()
            (nested / "beta.txt").write_text("fresh", encoding="utf-8")
            (root / "target.md").write_text("中文段落。", encoding="utf-8")

            progress = io.StringIO()
            with patched_batch_dependencies():
                plan = build_batch_plan(
                    root,
                    root / "out",
                    "zh",
                    fake_config(),
                    TemplateType.DEFAULT,
                    {},
                    recursive=True,
                    progress_stream=progress,
                )

            self.assertEqual(len(plan.files), 2)
            self.assertEqual(len(plan.skipped), 1)
            self.assertEqual(
                progress.getvalue().splitlines(),
                [
                    "Batch planning: scanned 3 file(s)",
                    "Batch planning: analyzing [1/3] alpha.md",
                    "Batch planning: analyzing [2/3] nested/beta.txt",
                    "Batch planning: analyzing [3/3] target.md",
                    "Batch planning complete: 2 selected, 1 skipped",
                ],
            )

    def test_build_batch_plan_rewrites_and_clears_planning_progress_for_tty(
        self,
    ) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / "alpha.md").write_text("fresh", encoding="utf-8")
            nested = root / "nested"
            nested.mkdir()
            (nested / "beta.txt").write_text("fresh", encoding="utf-8")
            (root / "target.md").write_text("中文段落。", encoding="utf-8")

            progress = TtyStringIO()
            with patched_batch_dependencies():
                plan = build_batch_plan(
                    root,
                    root / "out",
                    "zh",
                    fake_config(),
                    TemplateType.DEFAULT,
                    {},
                    recursive=True,
                    progress_stream=progress,
                )

            self.assertEqual(len(plan.files), 2)
            self.assertEqual(len(plan.skipped), 1)
            self.assertEqual(
                progress.getvalue(),
                "\rBatch planning: scanned 3 file(s)\033[K"
                "\rBatch planning: analyzing [1/3] alpha.md\033[K"
                "\rBatch planning: analyzing [2/3] nested/beta.txt\033[K"
                "\rBatch planning: analyzing [3/3] target.md\033[K"
                "\rBatch planning complete: 2 selected, 1 skipped\033[K\n",
            )

    def test_build_batch_plan_progress_total_uses_readable_count_after_read_failure(
        self,
    ) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / "invalid.md").write_bytes(b"\xff\xfe\xfd")
            (root / "valid.md").write_text("fresh", encoding="utf-8")

            progress = io.StringIO()
            warning = io.StringIO()
            with patched_batch_dependencies(), redirect_stderr(warning):
                plan = build_batch_plan(
                    root,
                    root / "out",
                    "zh",
                    fake_config(),
                    TemplateType.DEFAULT,
                    {},
                    progress_stream=progress,
                )

            self.assertEqual(
                [file.relative_path for file in plan.files], [Path("valid.md")]
            )
            self.assertEqual(
                progress.getvalue().splitlines(),
                [
                    "Batch planning: scanned 2 file(s)",
                    "Batch planning: analyzing [1/1] valid.md",
                    "Batch planning complete: 1 selected, 0 skipped",
                ],
            )
            self.assertIn(
                "Warning: skipping invalid.md: not valid UTF-8",
                warning.getvalue(),
            )

    def test_build_batch_plan_tty_progress_returns_to_column_zero_before_warning(
        self,
    ) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / "invalid.md").write_bytes(b"\xff\xfe\xfd")
            (root / "valid.md").write_text("fresh", encoding="utf-8")

            progress = TtyStringIO()
            with patched_batch_dependencies(), redirect_stderr(progress):
                plan = build_batch_plan(
                    root,
                    root / "out",
                    "zh",
                    fake_config(),
                    TemplateType.DEFAULT,
                    {},
                    progress_stream=progress,
                )

            self.assertEqual(
                [file.relative_path for file in plan.files], [Path("valid.md")]
            )
            self.assertEqual(
                progress.getvalue(),
                "\rBatch planning: scanned 2 file(s)\033[K"
                "\r\033[K\n"
                "Warning: skipping invalid.md: not valid UTF-8\n"
                "\rBatch planning: analyzing [1/1] valid.md\033[K"
                "\rBatch planning complete: 1 selected, 0 skipped\033[K\n",
            )

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
                    recursive=True,
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

    def test_build_batch_plan_rejects_path_unsafe_target_lang(self) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / "input.md").write_text("fresh", encoding="utf-8")

            with patched_batch_dependencies():
                with self.assertRaisesRegex(ValueError, "ASCII letters"):
                    build_batch_plan(
                        root,
                        root / "out",
                        "../../etc",
                        fake_config(),
                        TemplateType.DEFAULT,
                        {},
                    )
                with self.assertRaisesRegex(ValueError, "ASCII letters"):
                    build_batch_plan(
                        root,
                        root / "out",
                        "zh.Hant",
                        fake_config(),
                        TemplateType.DEFAULT,
                        {},
                    )

    def test_build_batch_plan_accepts_relative_directory_roots(self) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            base = Path(tmpdir)
            current = base / "current"
            current.mkdir()
            drafts = base / "drafts"
            drafts.mkdir()
            (drafts / "input.md").write_text("fresh", encoding="utf-8")

            previous_cwd = Path.cwd()
            os.chdir(current)
            try:
                with patched_batch_dependencies():
                    plan = build_batch_plan(
                        Path("../drafts"),
                        None,
                        "zh",
                        fake_config(),
                        TemplateType.DEFAULT,
                        {},
                    )
            finally:
                os.chdir(previous_cwd)

            self.assertEqual(plan.root, drafts.resolve())
            self.assertEqual(
                [file.relative_path for file in plan.files], [Path("input.md")]
            )
            self.assertEqual(plan.skipped, ())
            self.assertEqual(plan.files[0].output_path, drafts / "input.zh.md")

    def test_output_path_allows_hyphenated_target_lang(self) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            source_path = root / "input.md"
            source_path.write_text("fresh", encoding="utf-8")

            self.assertEqual(
                _output_path(source_path, root, root / "out", "zh-Hant"),
                root / "out" / "input.zh-Hant.md",
            )

    @unittest.skipUnless(hasattr(os, "symlink"), "requires symlink support")
    def test_build_batch_plan_skips_in_place_output_escape(self) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            base = Path(tmpdir)
            root = base / "root"
            root.mkdir()
            escape_dir = base / "escape"
            escape_dir.mkdir()
            (escape_dir / "input.md").write_text("fresh", encoding="utf-8")
            os.symlink(escape_dir, root / "linked")

            warning = io.StringIO()
            with patched_batch_dependencies(), redirect_stderr(warning):
                plan = build_batch_plan(
                    root,
                    None,
                    "zh",
                    fake_config(),
                    TemplateType.DEFAULT,
                    {},
                    recursive=True,
                )

            self.assertEqual(len(plan.files), 0)
            self.assertEqual(len(plan.skipped), 1)
            self.assertEqual(plan.skipped[0].relative_path, Path("linked/input.md"))
            self.assertEqual(plan.skipped[0].reason, "output path escapes scan root")
            self.assertIn(
                "Warning: skipping linked/input.md: output path escapes scan root",
                warning.getvalue(),
            )

    @unittest.skipUnless(hasattr(os, "symlink"), "requires symlink support")
    def test_build_batch_plan_skips_output_dir_escape(self) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            nested = root / "nested"
            nested.mkdir()
            (nested / "input.md").write_text("fresh", encoding="utf-8")
            output_dir = root / "out"
            output_dir.mkdir()
            escape_dir = root / "escape"
            escape_dir.mkdir()
            os.symlink(escape_dir, output_dir / "nested")

            warning = io.StringIO()
            with patched_batch_dependencies(), redirect_stderr(warning):
                plan = build_batch_plan(
                    root,
                    output_dir,
                    "zh",
                    fake_config(),
                    TemplateType.DEFAULT,
                    {},
                    recursive=True,
                )

            self.assertEqual(len(plan.files), 0)
            self.assertEqual(len(plan.skipped), 1)
            self.assertEqual(plan.skipped[0].relative_path, Path("nested/input.md"))
            self.assertEqual(
                plan.skipped[0].reason,
                "output path escapes output directory",
            )
            self.assertIn(
                "Warning: skipping nested/input.md: "
                "output path escapes output directory",
                warning.getvalue(),
            )

    @unittest.skipUnless(hasattr(os, "symlink"), "requires symlink support")
    def test_build_batch_plan_tty_progress_clears_before_output_escape_warning(
        self,
    ) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            nested = root / "nested"
            nested.mkdir()
            (nested / "input.md").write_text("fresh", encoding="utf-8")
            output_dir = root / "out"
            output_dir.mkdir()
            escape_dir = root / "escape"
            escape_dir.mkdir()
            os.symlink(escape_dir, output_dir / "nested")

            progress = TtyStringIO()
            with patched_batch_dependencies(), redirect_stderr(progress):
                plan = build_batch_plan(
                    root,
                    output_dir,
                    "zh",
                    fake_config(),
                    TemplateType.DEFAULT,
                    {},
                    recursive=True,
                    progress_stream=progress,
                )

            self.assertEqual(plan.files, ())
            self.assertEqual(len(plan.skipped), 1)
            self.assertEqual(
                progress.getvalue(),
                "\rBatch planning: scanned 1 file(s)\033[K"
                "\rBatch planning: analyzing [1/1] nested/input.md\033[K"
                "\r\033[K\n"
                "Warning: skipping nested/input.md: "
                "output path escapes output directory\n"
                "\rBatch planning complete: 0 selected, 1 skipped\033[K\n",
            )

    @unittest.skipUnless(hasattr(os, "symlink"), "requires symlink support")
    def test_build_batch_plan_skips_invalid_utf8_and_broken_symlink(self) -> None:
        with temporary_home(), tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / "valid.md").write_text("fresh", encoding="utf-8")
            (root / "invalid.md").write_bytes(b"\xff\xfe\xfd")
            os.symlink(root / "missing.md", root / "broken.md")

            warning = io.StringIO()
            with patched_batch_dependencies(), redirect_stderr(warning):
                plan = build_batch_plan(
                    root,
                    root / "out",
                    "zh",
                    fake_config(),
                    TemplateType.DEFAULT,
                    {},
                )

            self.assertEqual(
                [file.relative_path for file in plan.files], [Path("valid.md")]
            )
            self.assertEqual(plan.skipped, ())
            text = warning.getvalue()
            self.assertIn(
                "Warning: skipping invalid.md: not valid UTF-8",
                text,
            )
            self.assertIn(
                "Warning: skipping broken.md: broken symlink",
                text,
            )

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
    def test_batch_help_shows_recursive_flag(self) -> None:
        runner = CliRunner()

        result = runner.invoke(main, ["batch", "--help"])

        self.assertEqual(result.exit_code, 0, result.output)
        self.assertIn("--recursive", result.output)

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
            scanning_index = result.output.index("Batch planning: scanned 1 file(s)")
            analyzing_index = result.output.index(
                "Batch planning: analyzing [1/1] input.md"
            )
            complete_index = result.output.index(
                "Batch planning complete: 1 selected, 0 skipped"
            )
            preview_index = result.output.index("Batch root:")
            self.assertLess(scanning_index, preview_index)
            self.assertLess(analyzing_index, preview_index)
            self.assertLess(complete_index, preview_index)
            self.assertFalse((root / "input.zh.md").exists())

    def test_batch_recursive_scans_subdirectories(self) -> None:
        with temporary_home() as home, tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            nested = root / "nested"
            nested.mkdir()
            (nested / "input.md").write_text("English.", encoding="utf-8")

            runner = CliRunner()
            with (
                patched_batch_dependencies(),
                patch("hymt.cli.HotConfig", return_value=fake_config()),
            ):
                result = runner.invoke(
                    main,
                    ["batch", "-t", "zh", "--recursive", str(root)],
                    env={"HOME": home},
                )

            self.assertEqual(result.exit_code, 0, result.output)
            self.assertIn("nested/input.md", result.output)


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


class TtyStringIO(io.StringIO):
    def isatty(self) -> bool:
        return True


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
