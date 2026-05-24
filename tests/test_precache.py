from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
import io
import os
from pathlib import Path
import sqlite3
import tempfile
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from click.testing import CliRunner

from hymt.cli import main
from hymt.config import HotConfig
from hymt.precache import (
    PrecacheItem,
    PrecacheSummary,
    _capture_help,
    _discover_manpage_items,
    _discover_items,
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
                main,
                ["exec", "precache", "git", "docker", "--recursive", "--section", "1"],
            )

        self.assertEqual(result.exit_code, 0)
        precache.assert_called_once()
        self.assertTrue(precache.call_args.kwargs["recursive"])
        self.assertEqual(precache.call_args.kwargs["section"], "1")
        self.assertEqual(
            precache.call_args.kwargs["command_filters"], ("git", "docker")
        )
        self.assertIn("2/3 translated, 1 failed", result.stderr)

    def test_no_args_default_uses_recent_shell_history(self) -> None:
        with temporary_home() as home:
            history = Path(home) / ".zsh_history"
            history.write_text(
                ": 1770000000:0;git status\n"
                ": 1770000001:0;hymt exec -- pytest\n"
                ": 1770000002:0;kubectl get pods\n",
                encoding="utf-8",
            )
            config = HotConfig()
            progress = io.StringIO()

            with patch(
                "hymt.precache.shutil.which",
                side_effect=lambda command: f"/usr/bin/{command}",
            ):
                items = _discover_items(
                    config,
                    recursive=False,
                    section=None,
                    progress_stream=progress,
                )

        self.assertEqual(
            [item.label for item in items],
            ["man kubectl", "man git", "kubectl", "git"],
        )

    def test_explicit_filters_include_plugin_blocklisted_commands(self) -> None:
        with temporary_home():
            config = HotConfig()
            with patch(
                "hymt.precache.shutil.which",
                side_effect=lambda command: f"/usr/bin/{command}",
            ):
                items = _discover_items(
                    config,
                    recursive=False,
                    section=None,
                    command_filters=("docker",),
                    progress_stream=io.StringIO(),
                )

        self.assertEqual([item.label for item in items], ["man docker", "docker"])

    def test_recursive_discovery_uses_thread_pool_and_progress(self) -> None:
        class RecordingExecutor(ThreadPoolExecutor):
            used = False

            def __init__(self, *args: object, **kwargs: object) -> None:
                RecordingExecutor.used = True
                super().__init__(*args, **kwargs)

        progress = io.StringIO()
        observed_progress: list[str] = []

        def fake_help(args: tuple[str, ...], *, command_path: str | None = None) -> str:
            del args, command_path
            observed_progress.append(progress.getvalue())
            return """
Usage: tool <command>

Commands:
  build      Build artifacts
  test       Run tests
"""

        with temporary_home() as home:
            bin_dir = Path(home) / "bin"
            bin_dir.mkdir()
            git_path = bin_dir / "git"
            kubectl_path = bin_dir / "kubectl"
            for path in (git_path, kubectl_path):
                path.write_text("#!/bin/sh\n", encoding="utf-8")
                path.chmod(0o755)
            config = HotConfig()
            with (
                patch("hymt.precache.ThreadPoolExecutor", RecordingExecutor),
                patch("hymt.precache._capture_uncached_help", side_effect=fake_help),
                patch("hymt.precache.DiscoveryCache._ensure_schema") as ensure_schema,
                patch(
                    "hymt.precache.shutil.which",
                    side_effect=lambda command: str(
                        git_path if command == "git" else kubectl_path
                    ),
                ),
            ):
                items = _discover_items(
                    config,
                    recursive=True,
                    section=None,
                    command_filters=("git", "kubectl"),
                    progress_stream=progress,
                )

        self.assertTrue(RecordingExecutor.used)
        ensure_schema.assert_called_once()
        self.assertTrue(
            any("Discovering... [0/2]" in snapshot for snapshot in observed_progress)
        )
        self.assertIn("Discovering... [2/2]", progress.getvalue())
        self.assertIn(
            PrecacheItem("help", "git build", ("git", "build"), str(git_path)),
            items,
        )
        self.assertIn(
            PrecacheItem(
                "help",
                "kubectl test",
                ("kubectl", "test"),
                str(kubectl_path),
            ),
            items,
        )

    def test_recursive_discovery_continues_when_cache_unavailable(self) -> None:
        def fake_help(args: tuple[str, ...], *, command_path: str | None = None) -> str:
            del args, command_path
            return """
Usage: tool <command>

Commands:
  build      Build artifacts
"""

        with temporary_home() as home:
            command_path = Path(home) / "bin" / "tool"
            command_path.parent.mkdir()
            command_path.write_text("#!/bin/sh\n", encoding="utf-8")
            command_path.chmod(0o755)
            config = HotConfig()
            with (
                patch("hymt.precache._capture_uncached_help", side_effect=fake_help),
                patch(
                    "hymt.precache.DiscoveryCache._ensure_schema",
                    side_effect=sqlite3.OperationalError("readonly"),
                ),
                patch("hymt.precache.shutil.which", return_value=str(command_path)),
            ):
                items = _discover_items(
                    config,
                    recursive=True,
                    section=None,
                    command_filters=("tool",),
                    progress_stream=io.StringIO(),
                )

        self.assertIn(
            PrecacheItem("help", "tool build", ("tool", "build"), str(command_path)),
            items,
        )

    def test_help_discovery_cache_reuses_and_invalidates_by_file_stat(self) -> None:
        with temporary_home() as home:
            command_path = Path(home) / "bin" / "tool"
            command_path.parent.mkdir()
            command_path.write_text("#!/bin/sh\n", encoding="utf-8")
            command_path.chmod(0o755)
            config = HotConfig()
            del config
            first = SimpleNamespace(
                returncode=0,
                stdout=b"Usage: tool\nCommands:\n  run   Run it\n",
            )
            second = SimpleNamespace(
                returncode=0,
                stdout=b"Usage: tool\nCommands:\n  check   Check it\n",
            )

            with patch(
                "hymt.precache.subprocess.run", side_effect=[first, second]
            ) as run:
                self.assertIn(
                    "run",
                    _capture_help(("tool",), command_path=str(command_path)),
                )
                self.assertIn(
                    "run",
                    _capture_help(("tool",), command_path=str(command_path)),
                )
                command_path.write_text("#!/bin/sh\n# changed\n", encoding="utf-8")
                changed_time = time.time() + 10
                os.utime(command_path, (changed_time, changed_time))
                self.assertIn(
                    "check",
                    _capture_help(("tool",), command_path=str(command_path)),
                )

        self.assertEqual(run.call_count, 2)


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
        self._old_histfile = os.environ.get("HISTFILE")
        os.environ["HOME"] = self._tmpdir.name
        os.environ.pop("HISTFILE", None)
        return self._tmpdir.name

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        if self._old_home is None:
            os.environ.pop("HOME", None)
        else:
            os.environ["HOME"] = self._old_home
        if self._old_histfile is None:
            os.environ.pop("HISTFILE", None)
        else:
            os.environ["HISTFILE"] = self._old_histfile
        self._tmpdir.cleanup()


if __name__ == "__main__":
    unittest.main()
