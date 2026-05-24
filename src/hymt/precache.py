from __future__ import annotations

from collections.abc import Sequence
from collections import deque
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
import asyncio
import json
import os
import re
import shlex
import shutil
import sqlite3
import subprocess
import sys
import time
from typing import TextIO

from hymt.config import HotConfig
from hymt.docs import _capture_man
from hymt.exec_cache import translate_cached_text
from hymt.exec_wrapper import _decode_for_translation, _looks_binary
from hymt.history import format_duration


MAN_APROPOS_PATTERN = re.compile(
    r"(?P<name>[A-Za-z0-9_.:+-]+)\s+\((?P<section>[^)]+)\)"
)
SUBCOMMAND_PATTERN = re.compile(r"^\s{2,}(?P<name>[A-Za-z0-9][A-Za-z0-9_.:-]*)\b")
RECENT_HISTORY_LINE_LIMIT = 1000
RECENT_HISTORY_COMMAND_LIMIT = 100
SHELL_BUILTINS = {
    "alias",
    "bg",
    "cd",
    "dirs",
    "disown",
    "echo",
    "eval",
    "exec",
    "exit",
    "export",
    "fg",
    "hash",
    "history",
    "jobs",
    "popd",
    "pushd",
    "pwd",
    "read",
    "set",
    "shift",
    "source",
    "test",
    "trap",
    "type",
    "ulimit",
    "unalias",
    "unset",
    "wait",
}

DISCOVERY_SCHEMA = """
CREATE TABLE IF NOT EXISTS discovery_cache (
    command_path TEXT NOT NULL,
    file_mtime REAL NOT NULL,
    file_size INTEGER NOT NULL,
    help_output TEXT NOT NULL,
    subcommands TEXT NOT NULL,
    cached_at TEXT NOT NULL,
    PRIMARY KEY (command_path, file_mtime, file_size)
);
"""


@dataclass(frozen=True)
class PrecacheItem:
    kind: str
    label: str
    args: tuple[str, ...]
    command_path: str | None = None


@dataclass(frozen=True)
class PrecacheSummary:
    total: int
    translated: int
    failed: int


@dataclass(frozen=True)
class CommandTarget:
    name: str
    path: Path | None


@dataclass(frozen=True)
class CommandHelp:
    target: CommandTarget
    help_output: str
    subcommands: tuple[str, ...]


def run_precache(
    target_lang: str,
    config: HotConfig,
    *,
    recursive: bool = False,
    section: str | None = None,
    command_filters: Sequence[str] = (),
    progress_stream: TextIO = sys.stderr,
) -> PrecacheSummary:
    items = _discover_items(
        config,
        recursive=recursive,
        section=section,
        command_filters=command_filters,
        progress_stream=progress_stream,
    )
    progress = ItemProgress(len(items), progress_stream)
    translated = 0
    failed = 0
    for item in items:
        started = time.monotonic()
        try:
            text = _load_item_text(item)
            if text.strip():
                cache_command, cache_subcommand = _cache_identity(item)
                asyncio.run(
                    translate_cached_text(
                        cache_command,
                        cache_subcommand,
                        text,
                        target_lang,
                        config,
                    )
                )
                translated += 1
        except (OSError, RuntimeError, ValueError) as exc:
            failed += 1
            print(f"hymt precache: skipped {item.label}: {exc}", file=progress_stream)
        finally:
            progress.update(translated + failed, time.monotonic() - started)
    progress.finish()
    return PrecacheSummary(total=len(items), translated=translated, failed=failed)


def _discover_items(
    config: HotConfig,
    *,
    recursive: bool,
    section: str | None,
    command_filters: Sequence[str] = (),
    progress_stream: TextIO = sys.stderr,
) -> list[PrecacheItem]:
    targets = _discover_command_targets(config, command_filters)
    items = _discover_command_manpage_items(targets, section)
    help_items = [
        PrecacheItem(
            "help",
            target.name,
            (target.name,),
            str(target.path) if target.path is not None else None,
        )
        for target in targets
    ]
    items.extend(help_items)
    if recursive:
        command_help = _discover_command_help_parallel(targets, progress_stream)
        for target in targets:
            help_result = command_help.get(target.name)
            if help_result is None:
                continue
            for subcommand in help_result.subcommands:
                items.append(
                    PrecacheItem(
                        "help",
                        f"{target.name} {subcommand}",
                        (target.name, subcommand),
                        str(target.path) if target.path is not None else None,
                    )
                )
    return _deduplicate_items(items)


def _discover_command_targets(
    config: HotConfig, command_filters: Sequence[str]
) -> list[CommandTarget]:
    commands = (
        tuple(command_filters)
        if command_filters
        else tuple(_discover_recent_history_commands(config))
    )
    targets: list[CommandTarget] = []
    seen: set[str] = set()
    for command in commands:
        target = _resolve_command_target(command)
        if target is None or target.name in seen:
            continue
        targets.append(target)
        seen.add(target.name)
    return targets


def _discover_recent_history_commands(config: HotConfig) -> list[str]:
    skipped = set(config.exec_skip_commands) | set(config.exec_plugin_blocklist)
    commands: list[str] = []
    seen: set[str] = set()
    for path in _shell_history_paths():
        for command in _read_history_commands(path):
            if command in skipped or command in seen:
                continue
            commands.append(command)
            seen.add(command)
            if len(commands) >= RECENT_HISTORY_COMMAND_LIMIT:
                return commands
    return commands


def _shell_history_paths() -> tuple[Path, ...]:
    paths: list[Path] = []
    histfile = os.environ.get("HISTFILE")
    if histfile:
        paths.append(Path(histfile).expanduser())
    home = Path.home()
    paths.extend(
        [
            home / ".zsh_history",
            home / ".bash_history",
            home / ".local" / "share" / "fish" / "fish_history",
        ]
    )
    unique: dict[Path, None] = {}
    for path in paths:
        unique.setdefault(path, None)
    return tuple(unique.keys())


def _read_history_commands(path: Path) -> tuple[str, ...]:
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return ()
    commands: list[str] = []
    for line in reversed(lines[-RECENT_HISTORY_LINE_LIMIT:]):
        command = _extract_history_command(line)
        if command is not None:
            commands.append(command)
    return tuple(commands)


def _extract_history_command(line: str) -> str | None:
    raw_command = line.strip()
    if not raw_command or _is_bash_history_timestamp(raw_command):
        return None
    if raw_command.startswith(": ") and ";" in raw_command:
        raw_command = raw_command.split(";", 1)[1].strip()
    elif raw_command.startswith("- cmd:"):
        raw_command = raw_command.removeprefix("- cmd:").strip()
    try:
        tokens = shlex.split(raw_command)
    except ValueError:
        tokens = raw_command.split()
    command = _first_command_token(tokens)
    if command is None:
        return None
    return Path(command).name if _has_path_separator(command) else command


def _is_bash_history_timestamp(line: str) -> bool:
    return line.startswith("#") and line[1:].isdigit()


def _first_command_token(tokens: list[str]) -> str | None:
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if not token or token.startswith("-"):
            index += 1
            continue
        if "=" in token and not _has_path_separator(token):
            index += 1
            continue
        if token in {"sudo", "doas", "command", "builtin", "noglob"}:
            index += 1
            continue
        if token == "env":
            index += 1
            while index < len(tokens) and "=" in tokens[index]:
                index += 1
            continue
        if token in SHELL_BUILTINS:
            return None
        return token
    return None


def _has_path_separator(value: str) -> bool:
    return "/" in value or (os.altsep is not None and os.altsep in value)


def _resolve_command_target(command: str) -> CommandTarget | None:
    stripped = command.strip()
    if not stripped:
        return None
    if _has_path_separator(stripped):
        path = Path(stripped).expanduser()
        resolved = path.resolve() if path.exists() else None
        return CommandTarget(path.name, resolved)
    resolved_path = shutil.which(stripped)
    return CommandTarget(
        stripped,
        Path(resolved_path).resolve() if resolved_path is not None else None,
    )


def _discover_command_manpage_items(
    targets: Sequence[CommandTarget], section: str | None
) -> list[PrecacheItem]:
    items: list[PrecacheItem] = []
    for target in targets:
        if section is None:
            items.append(PrecacheItem("man", f"man {target.name}", (target.name,)))
        else:
            items.append(
                PrecacheItem(
                    "man",
                    f"man {section} {target.name}",
                    (section, target.name),
                )
            )
    return items


def _discover_manpage_items(section: str | None) -> list[PrecacheItem]:
    completed = subprocess.run(
        ["man", "-k", "."],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if completed.returncode != 0:
        message = completed.stderr.strip() or completed.stdout.strip()
        raise RuntimeError(message or "man -k . failed")
    items: list[PrecacheItem] = []
    for match in MAN_APROPOS_PATTERN.finditer(completed.stdout):
        page = match.group("name")
        page_section = match.group("section")
        if section is not None and page_section != section:
            continue
        items.append(
            PrecacheItem(
                "man",
                f"man {page_section} {page}",
                (page_section, page),
            )
        )
    return items


def _load_item_text(item: PrecacheItem) -> str:
    if item.kind == "man":
        return _capture_man(item.args)
    if item.kind == "help":
        return _capture_help(item.args, command_path=item.command_path)
    raise ValueError(f"Unsupported precache item kind: {item.kind}")


def _cache_identity(item: PrecacheItem) -> tuple[str, str]:
    if item.kind == "man":
        return "man", " ".join(item.args)
    if item.kind == "help":
        return item.args[0], " ".join(item.args[1:])
    return item.kind, " ".join(item.args)


def _capture_help(args: tuple[str, ...], *, command_path: str | None = None) -> str:
    if command_path is not None and len(args) == 1:
        cache = DiscoveryCache(user_discovery_cache_path())
        cache.initialize()
        result = _discover_command_help(
            CommandTarget(args[0], Path(command_path)), cache
        )
        cache.store(result)
        return result.help_output
    return _capture_uncached_help(args, command_path=command_path)


def _capture_uncached_help(
    args: tuple[str, ...], *, command_path: str | None = None
) -> str:
    command_args = _command_invocation_args(args, command_path)
    output = _run_help_command((*command_args, "--help"))
    if output.strip():
        return output
    return _run_help_command((*command_args, "-h"))


def _command_invocation_args(
    args: tuple[str, ...], command_path: str | None
) -> tuple[str, ...]:
    if command_path is None:
        return args
    return (command_path, *args[1:])


def _discover_command_help_parallel(
    targets: Sequence[CommandTarget], progress_stream: TextIO
) -> dict[str, CommandHelp]:
    targets_with_paths = [target for target in targets if target.path is not None]
    progress = DiscoveryProgress(len(targets_with_paths), progress_stream)
    progress.start()
    if not targets_with_paths:
        return {}
    results: dict[str, CommandHelp] = {}
    cache = DiscoveryCache(user_discovery_cache_path())
    cache.initialize()
    with ThreadPoolExecutor() as executor:
        futures = [
            executor.submit(_discover_command_help, target, cache)
            for target in targets_with_paths
        ]
        completed = 0
        for future in as_completed(futures):
            completed += 1
            try:
                result = future.result()
            except (OSError, RuntimeError, ValueError):
                progress.update(completed)
                continue
            results[result.target.name] = result
            cache.store(result)
            progress.update(completed)
    progress.finish()
    return results


def _discover_command_help(
    target: CommandTarget, cache: DiscoveryCache | None = None
) -> CommandHelp:
    if target.path is None:
        help_output = _capture_uncached_help((target.name,))
        return CommandHelp(target, help_output, _extract_subcommands(help_output))
    discovery_cache = (
        cache if cache is not None else DiscoveryCache(user_discovery_cache_path())
    )
    if cache is None:
        discovery_cache.initialize()
    cached = discovery_cache.find(target)
    if cached is not None:
        return cached
    help_output = _capture_uncached_help((target.name,), command_path=str(target.path))
    return CommandHelp(target, help_output, _extract_subcommands(help_output))


def user_discovery_cache_path() -> Path:
    return Path.home() / ".cache" / "hymt" / "discovery-cache.db"


def _run_help_command(args: tuple[str, ...]) -> str:
    try:
        completed = subprocess.run(
            list(args),
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=15,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(f"{args[0]} --help timed out") from exc
    if completed.returncode not in {0, 1, 2}:
        raise RuntimeError(f"{' '.join(args)} exited with {completed.returncode}")
    if _looks_binary(completed.stdout):
        return ""
    text = _decode_for_translation(completed.stdout)
    return text or ""


def _extract_subcommands(help_text: str) -> tuple[str, ...]:
    subcommands: list[str] = []
    in_commands = False
    for line in help_text.splitlines():
        normalized = line.strip().lower()
        if normalized in {"commands:", "subcommands:", "available commands:"}:
            in_commands = True
            continue
        if in_commands and line and not line.startswith((" ", "\t")):
            break
        if not in_commands:
            continue
        match = SUBCOMMAND_PATTERN.match(line)
        if match is None:
            continue
        name = match.group("name")
        if name not in {"help", "completion"}:
            subcommands.append(name)
    return tuple(dict.fromkeys(subcommands[:50]))


def _deduplicate_items(items: list[PrecacheItem]) -> list[PrecacheItem]:
    unique: dict[tuple[str, tuple[str, ...]], PrecacheItem] = {}
    for item in items:
        unique.setdefault((item.kind, item.args), item)
    return list(unique.values())


class DiscoveryCache:
    def __init__(self, path: Path | str) -> None:
        self._path = Path(path).expanduser()
        self._initialized = False
        self._available = True

    def initialize(self) -> None:
        if self._initialized or not self._available:
            return
        try:
            connection = self._connect(create=True)
        except (OSError, sqlite3.Error):
            self._available = False
            return
        try:
            self._ensure_schema(connection)
        except sqlite3.Error:
            self._available = False
        else:
            self._initialized = True
        finally:
            connection.close()

    def find(self, target: CommandTarget) -> CommandHelp | None:
        if (
            target.path is None
            or not self._initialized
            or not self._available
            or not self._path.exists()
        ):
            return None
        try:
            stat = target.path.stat()
        except OSError:
            return None
        try:
            connection = self._connect(create=False)
        except sqlite3.Error:
            self._available = False
            return None
        try:
            row = connection.execute(
                """
                SELECT help_output, subcommands
                FROM discovery_cache
                WHERE command_path = ?
                  AND file_mtime = ?
                  AND file_size = ?
                LIMIT 1
                """,
                (str(target.path), stat.st_mtime, stat.st_size),
            ).fetchone()
        except sqlite3.Error:
            self._available = False
            return None
        finally:
            connection.close()
        if row is None:
            return None
        subcommands = _decode_subcommands(row["subcommands"])
        return CommandHelp(target, str(row["help_output"]), subcommands)

    def store(self, result: CommandHelp) -> None:
        if result.target.path is None or not self._initialized or not self._available:
            return
        try:
            stat = result.target.path.stat()
        except OSError:
            return
        try:
            connection = self._connect(create=True)
        except (OSError, sqlite3.Error):
            self._available = False
            return
        try:
            command_path = str(result.target.path)
            file_mtime = stat.st_mtime
            file_size = stat.st_size
            connection.execute(
                """
                DELETE FROM discovery_cache
                WHERE command_path = ?
                  AND (file_mtime != ? OR file_size != ?)
                """,
                (command_path, file_mtime, file_size),
            )
            connection.execute(
                """
                INSERT INTO discovery_cache (
                    command_path,
                    file_mtime,
                    file_size,
                    help_output,
                    subcommands,
                    cached_at
                )
                VALUES (?, ?, ?, ?, ?, ?)
                ON CONFLICT(command_path, file_mtime, file_size)
                DO UPDATE SET
                    help_output = excluded.help_output,
                    subcommands = excluded.subcommands,
                    cached_at = excluded.cached_at
                """,
                (
                    command_path,
                    file_mtime,
                    file_size,
                    result.help_output,
                    json.dumps(list(result.subcommands), separators=(",", ":")),
                    datetime.now(timezone.utc).isoformat(timespec="seconds"),
                ),
            )
            connection.commit()
        except sqlite3.Error:
            self._available = False
        finally:
            connection.close()

    def _connect(self, *, create: bool) -> sqlite3.Connection:
        if create:
            self._path.parent.mkdir(parents=True, exist_ok=True)
        connection = sqlite3.connect(str(self._path))
        connection.row_factory = sqlite3.Row
        return connection

    def _ensure_schema(self, connection: sqlite3.Connection) -> None:
        connection.executescript(DISCOVERY_SCHEMA)
        connection.commit()


def _decode_subcommands(raw: str) -> tuple[str, ...]:
    try:
        decoded = json.loads(raw)
    except json.JSONDecodeError:
        return ()
    if not isinstance(decoded, list):
        return ()
    return tuple(item for item in decoded if isinstance(item, str))


class DiscoveryProgress:
    def __init__(self, total_items: int, stream: TextIO) -> None:
        self._total_items = total_items
        self._stream = stream
        self._uses_carriage_return = stream.isatty()
        self._printed = False

    def start(self) -> None:
        if self._total_items == 0:
            return
        self._write(0)

    def update(self, completed_items: int) -> None:
        if self._total_items == 0:
            return
        self._write(completed_items)

    def finish(self) -> None:
        if self._printed and self._uses_carriage_return:
            self._stream.write("\n")
            self._stream.flush()

    def _write(self, completed_items: int) -> None:
        line = (
            f"Discovering... [{completed_items}/{self._total_items}] "
            "command help/subcommands"
        )
        if self._uses_carriage_return:
            self._stream.write(f"\r{line}")
        else:
            self._stream.write(f"{line}\n")
        self._stream.flush()
        self._printed = True


class ItemProgress:
    def __init__(self, total_items: int, stream: TextIO) -> None:
        self._total_items = total_items
        self._stream = stream
        self._started_monotonic = time.monotonic()
        self._recent_item_seconds: deque[float] = deque(maxlen=10)
        self._uses_carriage_return = stream.isatty()
        self._printed = False

    def update(self, completed_items: int, item_seconds: float) -> None:
        if self._total_items == 0:
            return
        self._recent_item_seconds.append(max(0.0, item_seconds))
        elapsed = time.monotonic() - self._started_monotonic
        remaining = max(0, self._total_items - completed_items)
        eta_seconds = self._estimate_remaining_seconds(remaining)
        items_per_second = completed_items / elapsed if elapsed > 0 else 0.0
        percent = completed_items / self._total_items * 100
        line = (
            f"[{completed_items}/{self._total_items}] {percent:.2f}% | "
            f"elapsed {format_duration(elapsed)} | "
            f"eta {format_duration(eta_seconds)} | "
            f"{items_per_second:.2f} items/s"
        )
        if self._uses_carriage_return:
            self._stream.write(f"\r{line}")
        else:
            self._stream.write(f"{line}\n")
        self._stream.flush()
        self._printed = True

    def finish(self) -> None:
        if self._printed and self._uses_carriage_return:
            self._stream.write("\n")
            self._stream.flush()

    def _estimate_remaining_seconds(self, remaining_items: int) -> float:
        if remaining_items == 0 or not self._recent_item_seconds:
            return 0.0
        average_seconds = sum(self._recent_item_seconds) / len(
            self._recent_item_seconds
        )
        return average_seconds * remaining_items
