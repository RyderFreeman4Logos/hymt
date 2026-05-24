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
    DiscoveryProgress,
    ItemProgress,
    PrecacheItem,
    PrecacheSummary,
    RECENT_HISTORY_LINE_LIMIT,
    _capture_help,
    _discover_manpage_items,
    _discover_items,
    _extract_history_command,
    _extract_subcommands,
    _read_history_commands,
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

    def test_read_history_commands_streams_recent_lines(self) -> None:
        with temporary_home() as home:
            history = Path(home) / ".zsh_history"
            history.write_text(
                "".join(
                    f": {1770000000 + index}:0;cmd{index}\n"
                    for index in range(RECENT_HISTORY_LINE_LIMIT + 5)
                ),
                encoding="utf-8",
            )

            with patch(
                "pathlib.Path.read_text",
                side_effect=AssertionError("history must be streamed"),
            ):
                commands = _read_history_commands(history)

        self.assertEqual(len(commands), RECENT_HISTORY_LINE_LIMIT)
        self.assertEqual(commands[0], f"cmd{RECENT_HISTORY_LINE_LIMIT + 4}")
        self.assertEqual(commands[-1], "cmd5")
        self.assertNotIn("cmd0", commands)

    def test_read_history_commands_filters_fish_metadata(self) -> None:
        with temporary_home() as home:
            fish_dir = Path(home) / ".local" / "share" / "fish"
            fish_dir.mkdir(parents=True)
            history = fish_dir / "fish_history"
            history.write_text(
                "- cmd: git status\n"
                "  when: 1770000000\n"
                "  paths:\n"
                "    - /tmp/project\n"
                "- cmd: kubectl get pods\n"
                "  when: 1770000001\n",
                encoding="utf-8",
            )

            commands = _read_history_commands(history)

        self.assertEqual(commands, ("kubectl", "git"))
        self.assertNotIn("when:", commands)

    def test_extract_history_command_preserves_binary_paths(self) -> None:
        self.assertEqual(_extract_history_command("./bin/tool --help"), "./bin/tool")
        self.assertEqual(
            _extract_history_command("/opt/tools/my-tool run"), "/opt/tools/my-tool"
        )

    def test_extract_history_command_skips_privilege_wrapper_options(self) -> None:
        self.assertEqual(_extract_history_command("sudo -u root git status"), "git")
        self.assertEqual(_extract_history_command("sudo --user root git status"), "git")
        self.assertEqual(_extract_history_command("sudo -u=root git status"), "git")
        self.assertEqual(
            _extract_history_command("doas -u root ./scripts/deploy --check"),
            "./scripts/deploy",
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

    def test_tty_progress_updates_clear_previous_line(self) -> None:
        progress = TtyStringIO()

        discovery = DiscoveryProgress(10, progress)
        discovery.start()
        discovery.update(10)

        item = ItemProgress(10, progress)
        item.update(1, 0.1)
        item.update(10, 0.1)
        item.finish()

        output = progress.getvalue()
        self.assertIn("\r\033[KDiscovering... [0/10]", output)
        self.assertIn("\r\033[KDiscovering... [10/10]", output)
        self.assertIn("\r\033[K[1/10]", output)
        self.assertIn("\r\033[K[10/10]", output)

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
                changed_stat = command_path.stat()
                cache_path = Path(home) / ".cache" / "hymt" / "discovery-cache.db"
                with sqlite3.connect(cache_path) as connection:
                    rows = connection.execute(
                        """
                        SELECT command_path, file_mtime, file_size, help_output
                        FROM discovery_cache
                        WHERE command_path = ?
                        """,
                        (str(command_path),),
                    ).fetchall()

                self.assertEqual(len(rows), 1)
                self.assertEqual(rows[0][0], str(command_path))
                self.assertEqual(rows[0][1], changed_stat.st_mtime)
                self.assertEqual(rows[0][2], changed_stat.st_size)
                self.assertIn("check", rows[0][3])

        self.assertEqual(run.call_count, 2)

    def test_help_discovery_cache_keys_by_command_arguments(self) -> None:
        with temporary_home() as home:
            command_path = Path(home) / "bin" / "tool"
            command_path.parent.mkdir()
            command_path.write_text("#!/bin/sh\n", encoding="utf-8")
            command_path.chmod(0o755)
            top_level = SimpleNamespace(returncode=0, stdout=b"Usage: tool\n")
            subcommand = SimpleNamespace(returncode=0, stdout=b"Usage: tool run\n")

            with patch(
                "hymt.precache.subprocess.run",
                side_effect=[top_level, subcommand],
            ) as run:
                self.assertIn(
                    "Usage: tool",
                    _capture_help(("tool",), command_path=str(command_path)),
                )
                self.assertIn(
                    "Usage: tool run",
                    _capture_help(("tool", "run"), command_path=str(command_path)),
                )
                self.assertIn(
                    "Usage: tool",
                    _capture_help(("tool",), command_path=str(command_path)),
                )
                self.assertIn(
                    "Usage: tool run",
                    _capture_help(("tool", "run"), command_path=str(command_path)),
                )

            cache_path = Path(home) / ".cache" / "hymt" / "discovery-cache.db"
            with sqlite3.connect(cache_path) as connection:
                rows = connection.execute(
                    """
                    SELECT help_args, help_output
                    FROM discovery_cache
                    WHERE command_path = ?
                    ORDER BY help_args
                    """,
                    (str(command_path),),
                ).fetchall()

        self.assertEqual(run.call_count, 2)
        self.assertEqual(
            rows,
            [
                ('["tool","run"]', "Usage: tool run\n"),
                ('["tool"]', "Usage: tool\n"),
            ],
        )

    def test_help_discovery_cache_hit_does_not_store_again(self) -> None:
        with temporary_home() as home:
            command_path = Path(home) / "bin" / "tool"
            command_path.parent.mkdir()
            command_path.write_text("#!/bin/sh\n", encoding="utf-8")
            command_path.chmod(0o755)
            completed = SimpleNamespace(returncode=0, stdout=b"Usage: tool\n")

            with patch("hymt.precache.subprocess.run", return_value=completed) as run:
                _capture_help(("tool",), command_path=str(command_path))

            with (
                patch("hymt.precache.subprocess.run", return_value=completed) as run,
                patch(
                    "hymt.precache.DiscoveryCache.store",
                    side_effect=AssertionError("cache hit must not be stored"),
                ),
            ):
                self.assertIn(
                    "Usage: tool",
                    _capture_help(("tool",), command_path=str(command_path)),
                )

        run.assert_not_called()


async def fake_translate(
    command: str,
    subcommand: str,
    text: str,
    target_lang: str,
    config: HotConfig,
) -> str:
    del command, subcommand, target_lang, config
    return f"ZH:{text}"


class TtyStringIO(io.StringIO):
    def isatty(self) -> bool:
        return True


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
