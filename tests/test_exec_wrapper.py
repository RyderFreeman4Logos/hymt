from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import io
import os
import sys
import tempfile
import unittest
from unittest.mock import AsyncMock, patch

from hymt.config import HotConfig
from hymt.exec_wrapper import run_exec_command


class ExecWrapperTests(unittest.TestCase):
    def test_exec_preserves_original_output_and_translates_text_streams(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()

        with (
            temporary_home(),
            patch(
                "hymt.exec_wrapper.translate_cached_text", side_effect=fake_translate
            ),
            redirect_stdout(stdout),
            redirect_stderr(stderr),
        ):
            returncode = run_exec_command(
                [
                    sys.executable,
                    "-c",
                    "import sys; print('out'); print('err', file=sys.stderr)",
                ],
                "zh",
                HotConfig(),
            )

        self.assertEqual(returncode, 0)
        self.assertEqual(stdout.getvalue(), "out\n")
        self.assertIn("err\n", stderr.getvalue())
        self.assertIn("[hymt translated stderr]", stderr.getvalue())
        self.assertIn("ZH:err", stderr.getvalue())
        self.assertIn("[hymt translated stdout]", stderr.getvalue())
        self.assertIn("ZH:out", stderr.getvalue())

    def test_exec_skips_structured_stdout(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()

        with (
            temporary_home(),
            patch(
                "hymt.exec_wrapper.translate_cached_text", new_callable=AsyncMock
            ) as tx,
            redirect_stdout(stdout),
            redirect_stderr(stderr),
        ):
            returncode = run_exec_command(
                [sys.executable, "-c", "print('{\"ok\": true}')"],
                "zh",
                HotConfig(),
            )

        self.assertEqual(returncode, 0)
        self.assertEqual(stdout.getvalue(), '{"ok": true}\n')
        self.assertNotIn("[hymt translated stdout]", stderr.getvalue())
        tx.assert_not_awaited()

    def test_exec_returns_wrapped_command_status(self) -> None:
        with (
            temporary_home(),
            patch(
                "hymt.exec_wrapper.translate_cached_text", side_effect=fake_translate
            ),
            redirect_stdout(io.StringIO()),
            redirect_stderr(io.StringIO()),
        ):
            returncode = run_exec_command(
                [sys.executable, "-c", "import sys; print('bad'); sys.exit(7)"],
                "zh",
                HotConfig(),
            )

        self.assertEqual(returncode, 7)


async def fake_translate(
    command: str,
    subcommand: str,
    text: str,
    target_lang: str,
    config: HotConfig,
) -> str:
    del command, subcommand, target_lang, config
    return f"ZH:{text}"


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
