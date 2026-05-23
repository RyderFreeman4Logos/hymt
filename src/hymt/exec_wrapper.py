from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path
import asyncio
import fnmatch
import json
import os
import subprocess
import sys
import threading
from typing import BinaryIO, TextIO

from hymt.config import HotConfig
from hymt.exec_cache import translate_cached_text


ANSI_CYAN = "\033[36m"
ANSI_YELLOW = "\033[33m"
ANSI_RESET = "\033[0m"


@dataclass(frozen=True)
class ExecResult:
    returncode: int
    stdout: bytes
    stderr: bytes


def run_exec_command(
    command: list[str], target_lang: str, config: HotConfig | None = None
) -> int:
    if not command:
        raise ValueError("command is required after '--'")
    active_config = config or HotConfig()
    result = _run_command(command)
    try:
        asyncio.run(_translate_result(command, result, target_lang, active_config))
    except KeyboardInterrupt:
        print("hymt: translation cancelled.", file=sys.stderr)
    except (OSError, RuntimeError, ValueError) as exc:
        print(f"hymt: translation failed: {exc}", file=sys.stderr)
    return result.returncode


def _run_command(command: list[str]) -> ExecResult:
    try:
        process = subprocess.Popen(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            close_fds=True,
        )
    except FileNotFoundError as exc:
        raise ValueError(f"Command not found: {command[0]}") from exc
    assert process.stdout is not None
    assert process.stderr is not None
    stdout_chunks: list[bytes] = []
    stderr_chunks: list[bytes] = []
    stdout_thread = threading.Thread(
        target=_copy_pipe,
        args=(process.stdout, sys.stdout, stdout_chunks),
        daemon=True,
    )
    stderr_thread = threading.Thread(
        target=_copy_pipe,
        args=(process.stderr, sys.stderr, stderr_chunks),
        daemon=True,
    )
    stdout_thread.start()
    stderr_thread.start()
    returncode = process.wait()
    stdout_thread.join()
    stderr_thread.join()
    return ExecResult(returncode, b"".join(stdout_chunks), b"".join(stderr_chunks))


def _copy_pipe(pipe: BinaryIO, stream: TextIO, chunks: list[bytes]) -> None:
    while True:
        chunk = pipe.read(8192)
        if not chunk:
            return
        chunks.append(chunk)
        _write_bytes(stream, chunk)


def _write_bytes(stream: TextIO, data: bytes) -> None:
    buffer = getattr(stream, "buffer", None)
    if buffer is not None:
        buffer.write(data)
        buffer.flush()
        return
    stream.write(data.decode("utf-8", errors="replace"))
    stream.flush()


async def _translate_result(
    command: list[str], result: ExecResult, target_lang: str, config: HotConfig
) -> None:
    if config.exec_translate_stderr and result.stderr:
        stderr_text = _decode_for_translation(result.stderr)
        if stderr_text is not None:
            translated = await _translate_output(
                command, stderr_text, target_lang, config
            )
            _write_translation("stderr", translated, sys.stderr, ANSI_YELLOW)
    if _should_translate_stdout(command, result.stdout, target_lang, config):
        stdout_text = _decode_for_translation(result.stdout)
        if stdout_text is not None:
            stream = sys.stdout if sys.stdout.isatty() else sys.stderr
            translated = await _translate_output(
                command, stdout_text, target_lang, config
            )
            _write_translation("stdout", translated, stream, ANSI_CYAN)


async def _translate_output(
    command: list[str], text: str, target_lang: str, config: HotConfig
) -> str:
    cache_command, cache_subcommand = _cache_identity(command)
    return await translate_cached_text(
        cache_command,
        cache_subcommand,
        text,
        target_lang,
        config,
    )


def _cache_identity(command: list[str]) -> tuple[str, str]:
    executable = Path(command[0]).name
    subcommand = command[1] if len(command) > 1 else ""
    return executable, subcommand


def _write_translation(
    stream_name: str, translated: str, stream: TextIO, color: str
) -> None:
    use_color = stream.isatty()
    prefix = f"\n[hymt translated {stream_name}]\n"
    if use_color:
        stream.write(f"{color}{prefix}{translated}{ANSI_RESET}")
    else:
        stream.write(f"{prefix}{translated}")
    if not translated.endswith("\n"):
        stream.write("\n")
    stream.flush()


def _should_translate_stdout(
    command: list[str], output: bytes, target_lang: str, config: HotConfig
) -> bool:
    del target_lang
    if not output:
        return False
    mode = config.exec_translate_stdout
    if mode is False:
        return False
    if _command_matches(command, config.exec_skip_commands):
        return False
    if _matches_skip_pattern(command, config.exec_skip_patterns):
        return False
    if _looks_binary(output):
        return False
    text = _decode_for_translation(output)
    if text is None:
        return False
    if _looks_structured(text) or _looks_like_build_progress(text):
        return False
    return True


def _command_matches(command: list[str], names: Iterable[str]) -> bool:
    executable = Path(command[0]).name
    return executable in set(names)


def _matches_skip_pattern(command: list[str], patterns: Iterable[str]) -> bool:
    if not patterns:
        return False
    candidates = {
        Path(command[0]).name,
        command[0],
        " ".join(command),
    }
    return any(
        fnmatch.fnmatchcase(candidate, pattern)
        for pattern in patterns
        for candidate in candidates
    )


def _decode_for_translation(output: bytes) -> str | None:
    if _looks_binary(output):
        return None
    try:
        return output.decode("utf-8")
    except UnicodeDecodeError:
        return output.decode("utf-8", errors="replace")


def _looks_binary(output: bytes) -> bool:
    sample = output[:4096]
    if b"\x00" in sample:
        return True
    if not sample:
        return False
    allowed_controls = {7, 8, 9, 10, 12, 13, 27}
    control_count = sum(
        1 for byte in sample if byte < 32 and byte not in allowed_controls
    )
    return control_count / len(sample) > 0.05


def _looks_structured(text: str) -> bool:
    stripped = text.strip()
    if not stripped:
        return False
    if _looks_json(stripped) or _looks_xml(stripped):
        return True
    return _looks_yaml(stripped)


def _looks_json(text: str) -> bool:
    if text[0] not in "{[":
        return False
    try:
        json.loads(text)
    except json.JSONDecodeError:
        return False
    return True


def _looks_xml(text: str) -> bool:
    if not text.startswith(("<", "<?xml")):
        return False
    first_line = text.splitlines()[0]
    return ">" in first_line


def _looks_yaml(text: str) -> bool:
    lines = [
        line for line in text.splitlines() if line.strip() and not line.startswith("#")
    ]
    if not lines:
        return False
    if lines[0].strip() in {"---", "..."}:
        return True
    key_value_lines = sum(1 for line in lines[:20] if ":" in line.split("#", 1)[0])
    return len(lines) >= 3 and key_value_lines / min(len(lines), 20) >= 0.8


def _looks_like_build_progress(text: str) -> bool:
    progress_prefixes = (
        "compiling ",
        "checking ",
        "building ",
        "running ",
        "finished ",
        "linking ",
        "generating ",
    )
    lines = [line.strip().lower() for line in text.splitlines() if line.strip()]
    if not lines:
        return False
    if any("error" in line or "warning" in line for line in lines):
        return False
    return all(line.startswith(progress_prefixes) for line in lines)


def is_agent_descendant() -> bool:
    agent_names = frozenset(
        {
            "claude",
            "claude-code",
            "csa",
            "codex",
            "gemini-cli",
            "gemini",
            "opencode",
            "aider",
            "cursor",
            "copilot",
            "antigravity-cli",
        }
    )
    pid = os.getpid()
    while pid > 1:
        try:
            proc = Path(f"/proc/{pid}")
            comm = (proc / "comm").read_text(encoding="utf-8").strip().lower()
            if comm in agent_names:
                return True
            cmdline = (proc / "cmdline").read_bytes()
            cmdline_text = cmdline.replace(b"\x00", b" ").decode(
                "utf-8", errors="replace"
            )
            if any(name in cmdline_text.lower() for name in agent_names):
                return True
            status = (proc / "status").read_text(encoding="utf-8")
            parent = _parent_pid(status)
            if parent is None:
                return False
            pid = parent
        except (OSError, ValueError):
            return False
    return False


def _parent_pid(status: str) -> int | None:
    for line in status.splitlines():
        if line.startswith("PPid:"):
            return int(line.split()[1])
    return None
