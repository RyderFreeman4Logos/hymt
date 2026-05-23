from __future__ import annotations

import asyncio
from contextlib import redirect_stdout
import io
import os
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

from click.testing import CliRunner

from hymt.cli import main
from hymt.templates import TemplateType
from hymt.translate import translate_file


class StdoutNewlineTests(unittest.TestCase):
    def test_translate_file_stdout_adds_missing_trailing_newline(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            input_path = Path(tmpdir) / "input.txt"
            input_path.write_text("source", encoding="utf-8")
            stdout = io.StringIO()

            with (
                patch(
                    "hymt.translate.translate_text",
                    new=AsyncMock(return_value="translated"),
                ),
                redirect_stdout(stdout),
            ):
                asyncio.run(
                    translate_file(
                        input_path,
                        None,
                        "en",
                        SimpleNamespace(),
                        TemplateType.DEFAULT,
                    )
                )

        self.assertEqual(stdout.getvalue(), "translated\n")

    def test_cli_stdout_adds_missing_trailing_newline(self) -> None:
        with temporary_home() as home:
            runner = CliRunner()
            with (
                patch("hymt.cli.HotConfig", return_value=SimpleNamespace()),
                patch("hymt.cli._announce_tokenizer_download"),
                patch(
                    "hymt.cli.translate_text", new=AsyncMock(return_value="translated")
                ),
            ):
                result = runner.invoke(main, ["-t", "en", "source"], env={"HOME": home})

        self.assertEqual(result.exit_code, 0, result.output)
        self.assertEqual(result.output, "translated\n")

    def test_cli_separator_treats_subcommand_name_as_text(self) -> None:
        with temporary_home() as home:
            runner = CliRunner()
            translate = AsyncMock(return_value="translated")
            with (
                patch("hymt.cli.HotConfig", return_value=SimpleNamespace()),
                patch("hymt.cli._announce_tokenizer_download"),
                patch("hymt.cli.translate_text", new=translate),
            ):
                result = runner.invoke(
                    main, ["-t", "en", "--", "config"], env={"HOME": home}
                )

        self.assertEqual(result.exit_code, 0, result.output)
        self.assertEqual(result.output, "translated\n")
        translate.assert_awaited_once()
        self.assertEqual(translate.await_args.args[0], "config")

    def test_cli_output_write_error_becomes_click_error(self) -> None:
        with temporary_home() as home, tempfile.TemporaryDirectory() as tmpdir:
            output_path = Path(tmpdir) / "existing-dir"
            output_path.mkdir()
            runner = CliRunner()
            with (
                patch("hymt.cli.HotConfig", return_value=SimpleNamespace()),
                patch("hymt.cli._announce_tokenizer_download"),
                patch(
                    "hymt.cli.translate_text", new=AsyncMock(return_value="translated")
                ),
            ):
                result = runner.invoke(
                    main,
                    ["-t", "en", "-o", str(output_path), "source"],
                    env={"HOME": home},
                )

        self.assertNotEqual(result.exit_code, 0)
        self.assertIn("Error:", result.output)


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


if __name__ == "__main__":
    unittest.main()
