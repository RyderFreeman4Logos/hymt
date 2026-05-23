from __future__ import annotations

from collections.abc import Sequence
from datetime import datetime, timezone
import asyncio
import os
import re
import shlex
import subprocess
import sys

from hymt.config import HotConfig
from hymt.templates import TemplateType
from hymt.translate import translate_text


def show_translated_man(
    args: Sequence[str],
    target_lang: str,
    config: HotConfig,
    *,
    original: bool = False,
    refresh: bool = False,
) -> int:
    if not args:
        raise ValueError("man page or man arguments are required")
    if original:
        return subprocess.call(["man", *args])
    text = _capture_man(args)
    translated = _translate_document(text, target_lang, config, refresh)
    return _page_text(translated)


def show_translated_info(
    args: Sequence[str],
    target_lang: str,
    config: HotConfig,
    *,
    original: bool = False,
    refresh: bool = False,
) -> int:
    if not args:
        raise ValueError("info topic or info arguments are required")
    if original:
        return subprocess.call(["info", *args])
    text = _capture_info(args)
    translated = _translate_document(text, target_lang, config, refresh)
    return _page_text(translated)


def _capture_man(args: Sequence[str]) -> str:
    env = os.environ.copy()
    env["MANPAGER"] = "cat"
    env["PAGER"] = "cat"
    env.setdefault("MANWIDTH", "100")
    return _capture_command(["man", *args], env)


def _capture_info(args: Sequence[str]) -> str:
    return _capture_command(["info", "--output=-", *args], os.environ.copy())


def _capture_command(command: list[str], env: dict[str, str]) -> str:
    completed = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=env,
    )
    if completed.returncode != 0:
        message = completed.stderr.strip() or completed.stdout.strip()
        raise RuntimeError(
            message or f"{command[0]} exited with {completed.returncode}"
        )
    return _strip_overstrikes(completed.stdout)


def _strip_overstrikes(text: str) -> str:
    previous = None
    cleaned = text
    while previous != cleaned:
        previous = cleaned
        cleaned = re.sub(r".\x08", "", cleaned)
    return cleaned


def _translate_document(
    text: str, target_lang: str, config: HotConfig, refresh: bool
) -> str:
    if refresh:
        return asyncio.run(
            translate_text(
                text,
                target_lang,
                config,
                TemplateType.DEFAULT,
                stream=False,
                cache_bust=datetime.now(timezone.utc).isoformat(timespec="seconds"),
            )
        )
    return asyncio.run(
        translate_text(
            text,
            target_lang,
            config,
            TemplateType.DEFAULT,
            stream=False,
        )
    )


def _page_text(text: str) -> int:
    output = text if text.endswith("\n") else f"{text}\n"
    if not sys.stdout.isatty():
        sys.stdout.write(output)
        sys.stdout.flush()
        return 0
    pager = os.environ.get("PAGER") or "less -R"
    command = shlex.split(pager)
    if not command:
        sys.stdout.write(output)
        sys.stdout.flush()
        return 0
    try:
        process = subprocess.Popen(command, stdin=subprocess.PIPE, text=True)
    except OSError:
        sys.stdout.write(output)
        sys.stdout.flush()
        return 0
    assert process.stdin is not None
    try:
        process.communicate(output)
    except BrokenPipeError:
        return process.wait()
    return process.returncode or 0
