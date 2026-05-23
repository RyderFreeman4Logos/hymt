from __future__ import annotations

import io
import os
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from click.testing import CliRunner

from hymt.cli import main
from hymt.config import HotConfig
from hymt.precache import (
    PrecacheItem,
    PrecacheSummary,
    _discover_manpage_items,
    _extract_subcommands,
    run_precache,
)


class PrecacheTests(unittest.TestCase):
    def test_discover_manpage_items_filters_section(self) -> None:
        completed = SimpleNamespace(
            returncode=0,
            stdout="printf (1) - format and print data\nprintf (3) - libc function\n",
            stderr="",
        )

        with patch("hymt.precache.subprocess.run", return_value=completed):
            items = _discover_manpage_items("1")

        self.assertEqual(items, [PrecacheItem("man", "man 1 printf", ("1", "printf"))])

    def test_extract_subcommands_from_commands_section(self) -> None:
        help_text = """
Usage: tool <command>

Commands:
  build      Build artifacts
  test       Run tests
  help       Show help

Options:
  -h, --help
"""
        self.assertEqual(_extract_subcommands(help_text), ("build", "test"))

    def test_run_precache_translates_discovered_items_with_progress(self) -> None:
        progress = io.StringIO()
        items = [
            PrecacheItem("man", "man 1 git", ("1", "git")),
            PrecacheItem("help", "git", ("git",)),
        ]

        with (
            temporary_home(),
            patch("hymt.precache._discover_items", return_value=items),
            patch("hymt.precache._load_item_text", side_effect=["manual", "help"]),
            patch("hymt.precache.translate_cached_text", side_effect=fake_translate),
        ):
            summary = run_precache("zh", HotConfig(), progress_stream=progress)

        self.assertEqual(summary, PrecacheSummary(total=2, translated=2, failed=0))
        self.assertIn("[2/2] 100.00%", progress.getvalue())
        self.assertIn("items/s", progress.getvalue())

    def test_cli_exec_precache_passes_options(self) -> None:
        runner = CliRunner()
        with patch(
            "hymt.cli.run_precache",
            return_value=PrecacheSummary(total=3, translated=2, failed=1),
        ) as precache:
            result = runner.invoke(
                main, ["exec", "precache", "--recursive", "--section", "1"]
            )

        self.assertEqual(result.exit_code, 0)
        precache.assert_called_once()
        self.assertTrue(precache.call_args.kwargs["recursive"])
        self.assertEqual(precache.call_args.kwargs["section"], "1")
        self.assertIn("2/3 translated, 1 failed", result.stderr)


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
