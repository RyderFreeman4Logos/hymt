from __future__ import annotations

from collections import deque
from dataclasses import dataclass
from pathlib import Path
import asyncio
import os
import re
import subprocess
import sys
import time
from typing import TextIO

from hymt.config import HotConfig
from hymt.docs import _capture_man
from hymt.exec_wrapper import _decode_for_translation, _looks_binary
from hymt.history import format_duration
from hymt.templates import TemplateType
from hymt.translate import translate_text


MAN_APROPOS_PATTERN = re.compile(
    r"(?P<name>[A-Za-z0-9_.:+-]+)\s+\((?P<section>[^)]+)\)"
)
SUBCOMMAND_PATTERN = re.compile(r"^\s{2,}(?P<name>[A-Za-z0-9][A-Za-z0-9_.:-]*)\b")


@dataclass(frozen=True)
class PrecacheItem:
    kind: str
    label: str
    args: tuple[str, ...]


@dataclass(frozen=True)
class PrecacheSummary:
    total: int
    translated: int
    failed: int


def run_precache(
    target_lang: str,
    config: HotConfig,
    *,
    recursive: bool = False,
    section: str | None = None,
    progress_stream: TextIO = sys.stderr,
) -> PrecacheSummary:
    items = _discover_items(config, recursive=recursive, section=section)
    progress = ItemProgress(len(items), progress_stream)
    translated = 0
    failed = 0
    for item in items:
        started = time.monotonic()
        try:
            text = _load_item_text(item)
            if text.strip():
                asyncio.run(
                    translate_text(
                        text,
                        target_lang,
                        config,
                        TemplateType.DEFAULT,
                        stream=False,
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
    config: HotConfig, *, recursive: bool, section: str | None
) -> list[PrecacheItem]:
    items = _discover_manpage_items(section)
    commands = _discover_path_commands(config)
    help_items = [PrecacheItem("help", command, (command,)) for command in commands]
    items.extend(help_items)
    if recursive:
        for item in help_items:
            try:
                help_text = _capture_help(item.args)
            except (OSError, RuntimeError):
                continue
            for subcommand in _extract_subcommands(help_text):
                items.append(
                    PrecacheItem(
                        "help",
                        f"{item.label} {subcommand}",
                        (*item.args, subcommand),
                    )
                )
    return _deduplicate_items(items)


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


def _discover_path_commands(config: HotConfig) -> list[str]:
    skipped = set(config.exec_skip_commands) | set(config.exec_plugin_blocklist)
    commands: set[str] = set()
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        if not directory:
            continue
        path = Path(directory)
        try:
            entries = list(path.iterdir())
        except OSError:
            continue
        for entry in entries:
            if entry.name in skipped or entry.name.startswith("."):
                continue
            try:
                if entry.is_file() and os.access(entry, os.X_OK):
                    commands.add(entry.name)
            except OSError:
                continue
    return sorted(commands)


def _load_item_text(item: PrecacheItem) -> str:
    if item.kind == "man":
        return _capture_man(item.args)
    if item.kind == "help":
        return _capture_help(item.args)
    raise ValueError(f"Unsupported precache item kind: {item.kind}")


def _capture_help(args: tuple[str, ...]) -> str:
    output = _run_help_command((*args, "--help"))
    if output.strip():
        return output
    return _run_help_command((*args, "-h"))


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
