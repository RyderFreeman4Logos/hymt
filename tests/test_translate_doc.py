from __future__ import annotations

import asyncio
from contextlib import redirect_stderr
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import AsyncMock, patch

from click.testing import CliRunner

from hymt.cli import main
from hymt.doc_translate import (
    DocTranslationTarget,
    _SourceState,
    _translate_target_until_stable,
    _wait_for_path_change,
    build_doc_translation_targets,
    run_doc_translation,
)
from hymt.templates import TemplateType


class TranslateDocTargetTests(unittest.TestCase):
    def test_build_targets_uses_zh_cn_suffix_for_default_chinese(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            source = Path(tmpdir) / "README.md"
            source.write_text("# hello\n", encoding="utf-8")

            targets = build_doc_translation_targets(source, "zh")

        self.assertEqual(len(targets), 1)
        self.assertEqual(targets[0].output_path.name, "README.zh-cn.md")

    def test_build_targets_scans_directories_recursively(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / "README.md").write_text("# root\n", encoding="utf-8")
            nested = root / "docs"
            nested.mkdir()
            (nested / "GUIDE.md").write_text("# guide\n", encoding="utf-8")
            (nested / "GUIDE.zh-cn.md").write_text("# 已翻译\n", encoding="utf-8")

            non_recursive = build_doc_translation_targets(root, "zh", recursive=False)
            recursive = build_doc_translation_targets(root, "zh", recursive=True)

        self.assertEqual(
            [target.source_path.name for target in non_recursive], ["README.md"]
        )
        self.assertEqual(
            [target.source_path.name for target in recursive],
            ["README.md", "GUIDE.md"],
        )

    @unittest.skipUnless(hasattr(os, "symlink"), "requires symlink support")
    def test_build_targets_skip_invalid_utf8_and_broken_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / "README.md").write_text("# hello\n", encoding="utf-8")
            (root / "BROKEN.md").write_bytes(b"\xff\xfe\xfd")
            os.symlink(root / "missing.md", root / "MISSING.md")

            warning = io.StringIO()
            with redirect_stderr(warning):
                targets = build_doc_translation_targets(root, "zh", recursive=False)

        self.assertEqual([target.source_path.name for target in targets], ["README.md"])
        text = warning.getvalue()
        self.assertIn("Warning: skipping BROKEN.md: not valid UTF-8", text)
        self.assertIn("Warning: skipping MISSING.md: broken symlink", text)


class TranslateDocRuntimeTests(unittest.TestCase):
    def test_run_doc_translation_writes_single_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            source = Path(tmpdir) / "README.md"
            source.write_text("# hello\n", encoding="utf-8")
            output = Path(tmpdir) / "README.zh-cn.md"
            fake_config = object()

            with patch(
                "hymt.doc_translate.translate_text",
                AsyncMock(return_value="# 你好\n"),
            ) as translate:
                run_doc_translation(
                    source,
                    "zh",
                    fake_config,  # type: ignore[arg-type]
                    output_path=output,
                    progress_stream=io.StringIO(),
                )

            self.assertTrue(output.exists())
            self.assertEqual(output.read_text(encoding="utf-8"), "# 你好\n")
            self.assertEqual(translate.await_count, 1)

    def test_translate_target_retries_when_source_changes_mid_translation(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            source = Path(tmpdir) / "README.md"
            output = Path(tmpdir) / "README.zh-cn.md"
            source.write_text("first", encoding="utf-8")
            target = DocTranslationTarget(source, output)
            progress = io.StringIO()

            class FakeConfig:
                max_retranslation_retries = 2

            calls = 0

            async def fake_translate_target_once(
                *args: object, **kwargs: object
            ) -> str:
                nonlocal calls
                calls += 1
                if calls == 1:
                    await asyncio.sleep(0.05)
                    return "stale"
                return "fresh"

            async def fake_wait_for_change(path: Path, state: _SourceState) -> None:
                del state
                if calls == 0:
                    await asyncio.sleep(0.01)
                    path.write_text("second", encoding="utf-8")
                    return
                await asyncio.sleep(3600)

            with (
                patch(
                    "hymt.doc_translate._translate_target_once",
                    side_effect=fake_translate_target_once,
                ),
                patch(
                    "hymt.doc_translate._wait_for_path_change",
                    side_effect=fake_wait_for_change,
                ),
            ):
                asyncio.run(
                    _translate_target_until_stable(
                        target,
                        "zh",
                        FakeConfig(),  # type: ignore[arg-type]
                        stream=None,
                        template_type=TemplateType.DEFAULT,
                        template_kwargs={},
                        progress_stream=progress,
                    )
                )

            self.assertEqual(output.read_text(encoding="utf-8"), "fresh")
            self.assertEqual(calls, 2)
            self.assertIn("retrying (1/2)", progress.getvalue())

    def test_wait_for_path_change_polls_without_watchfiles(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            source = Path(tmpdir) / "README.md"
            source.write_text("first", encoding="utf-8")
            initial_state = _SourceState(
                True,
                source.stat().st_mtime_ns,
                source.stat().st_size,
            )

            async def mutate() -> None:
                await asyncio.sleep(0.01)
                source.write_text("second", encoding="utf-8")

            async def wait_for_change() -> None:
                await asyncio.gather(
                    _wait_for_path_change(source, initial_state),
                    mutate(),
                )

            with patch("hymt.doc_translate._load_watchfiles", return_value=None):
                asyncio.run(wait_for_change())


class TranslateDocCliTests(unittest.TestCase):
    def test_cli_translate_doc_passes_arguments(self) -> None:
        runner = CliRunner()
        with patch("hymt.cli.run_doc_translation") as run:
            result = runner.invoke(
                main,
                [
                    "translate-doc",
                    "README.md",
                    "-t",
                    "ja",
                    "--watch",
                    "--stream",
                ],
            )

        self.assertEqual(result.exit_code, 0, result.output)
        run.assert_called_once()
        self.assertEqual(run.call_args.args[0], Path("README.md"))
        self.assertEqual(run.call_args.args[1], "ja")
        self.assertTrue(run.call_args.kwargs["watch"])
        self.assertTrue(run.call_args.kwargs["stream"])


if __name__ == "__main__":
    unittest.main()
