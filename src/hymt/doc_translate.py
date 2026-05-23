from __future__ import annotations

from collections.abc import Sequence
import asyncio
from dataclasses import dataclass
from importlib import import_module
import os
from pathlib import Path
import sys
import tempfile
from types import ModuleType
from typing import TextIO

from hymt.config import HotConfig
from hymt.language import analyze_document_language
from hymt.templates import TemplateType
from hymt.translate import translate_text


__all__ = [
    "DocTranslationTarget",
    "build_doc_translation_targets",
    "run_doc_translation",
]


MARKDOWN_SUFFIXES = frozenset({".md"})
TARGET_SUFFIX_ALIASES = {"zh": "zh-cn"}
WATCH_POLL_SECONDS = 0.25


@dataclass(frozen=True)
class DocTranslationTarget:
    source_path: Path
    output_path: Path


@dataclass(frozen=True)
class _SourceState:
    exists: bool
    mtime_ns: int
    size: int


def run_doc_translation(
    source: Path,
    target_lang: str,
    config: HotConfig,
    *,
    output_path: Path | None = None,
    output_dir: Path | None = None,
    recursive: bool = False,
    watch: bool = False,
    stream: bool | None = None,
    template_type: TemplateType = TemplateType.DEFAULT,
    template_kwargs: dict[str, object] | None = None,
    progress_stream: TextIO,
) -> None:
    targets = build_doc_translation_targets(
        source,
        target_lang,
        output_path=output_path,
        output_dir=output_dir,
        recursive=recursive,
    )
    if source.expanduser().is_dir():
        if watch:
            raise ValueError("--watch requires a single Markdown file")
        if output_path is not None:
            raise ValueError("--output is only supported for a single file")
        asyncio.run(
            _translate_targets(
                targets,
                target_lang,
                config,
                stream=stream,
                template_type=template_type,
                template_kwargs=template_kwargs or {},
                progress_stream=progress_stream,
            )
        )
        return
    if len(targets) != 1:
        raise ValueError("translate-doc expected exactly one file target")
    target = targets[0]
    if watch:
        asyncio.run(
            _watch_target(
                target,
                target_lang,
                config,
                stream=stream,
                template_type=template_type,
                template_kwargs=template_kwargs or {},
                progress_stream=progress_stream,
            )
        )
        return
    asyncio.run(
        _translate_target_until_stable(
            target,
            target_lang,
            config,
            stream=stream,
            template_type=template_type,
            template_kwargs=template_kwargs or {},
            progress_stream=progress_stream,
        )
    )


def build_doc_translation_targets(
    source: Path,
    target_lang: str,
    *,
    output_path: Path | None = None,
    output_dir: Path | None = None,
    recursive: bool = False,
) -> tuple[DocTranslationTarget, ...]:
    resolved_source = source.expanduser().resolve(strict=True)
    target_suffix = _target_lang_path_suffix(target_lang)
    if resolved_source.is_file():
        _validate_markdown_source(resolved_source)
        if output_path is not None and output_dir is not None:
            raise ValueError("Use either --output or --output-dir, not both")
        resolved_output = _file_output_path(
            resolved_source,
            resolved_source.parent,
            output_path,
            output_dir,
            target_suffix,
        )
        return (DocTranslationTarget(resolved_source, resolved_output),)

    if not resolved_source.is_dir():
        raise ValueError(f"Unsupported translate-doc source: {resolved_source}")
    if output_path is not None:
        raise ValueError("--output is only supported for a single file")

    files = _scan_markdown_files(resolved_source, recursive=recursive)
    targets: list[DocTranslationTarget] = []
    for path in files:
        if path.stem.endswith(f".{target_suffix}"):
            continue
        if not _is_utf8_text_file(path):
            print(
                f"Warning: skipping {path.relative_to(resolved_source)}: not valid UTF-8",
                file=sys.stderr,
            )
            continue
        targets.append(
            DocTranslationTarget(
                path,
                _file_output_path(
                    path, resolved_source, None, output_dir, target_suffix
                ),
            )
        )
    return tuple(targets)


async def _translate_targets(
    targets: Sequence[DocTranslationTarget],
    target_lang: str,
    config: HotConfig,
    *,
    stream: bool | None,
    template_type: TemplateType,
    template_kwargs: dict[str, object],
    progress_stream: TextIO,
) -> None:
    if not targets:
        print("No Markdown files selected.", file=progress_stream)
        return
    for index, target in enumerate(targets, start=1):
        print(
            f"Document {index}/{len(targets)}: {target.source_path} -> {target.output_path}",
            file=progress_stream,
        )
        await _translate_target_until_stable(
            target,
            target_lang,
            config,
            stream=stream,
            template_type=template_type,
            template_kwargs=template_kwargs,
            progress_stream=progress_stream,
        )


async def _watch_target(
    target: DocTranslationTarget,
    target_lang: str,
    config: HotConfig,
    *,
    stream: bool | None,
    template_type: TemplateType,
    template_kwargs: dict[str, object],
    progress_stream: TextIO,
) -> None:
    print(
        f"Watching {target.source_path} -> {target.output_path}",
        file=progress_stream,
    )
    last_state = await _translate_target_until_stable(
        target,
        target_lang,
        config,
        stream=stream,
        template_type=template_type,
        template_kwargs=template_kwargs,
        progress_stream=progress_stream,
    )
    while True:
        await _wait_for_path_change(target.source_path, last_state)
        last_state = await _translate_target_until_stable(
            target,
            target_lang,
            config,
            stream=stream,
            template_type=template_type,
            template_kwargs=template_kwargs,
            progress_stream=progress_stream,
        )


async def _translate_target_until_stable(
    target: DocTranslationTarget,
    target_lang: str,
    config: HotConfig,
    *,
    stream: bool | None,
    template_type: TemplateType,
    template_kwargs: dict[str, object],
    progress_stream: TextIO,
) -> _SourceState:
    retries = 0
    while True:
        initial_state = _source_state(target.source_path)
        if not initial_state.exists:
            raise FileNotFoundError(str(target.source_path))
        change_task = asyncio.create_task(
            _wait_for_path_change(target.source_path, initial_state)
        )
        translate_task = asyncio.create_task(
            _translate_target_once(
                target,
                target_lang,
                config,
                stream=stream,
                template_type=template_type,
                template_kwargs=template_kwargs,
            )
        )
        done, _pending = await asyncio.wait(
            {change_task, translate_task},
            return_when=asyncio.FIRST_COMPLETED,
        )
        if change_task in done and translate_task not in done:
            translate_task.cancel()
            await _suppress_cancellation(translate_task)
            retries += 1
            _raise_if_retry_exhausted(config, retries)
            print(
                f"Source changed during translation; retrying ({retries}/{config.max_retranslation_retries})",
                file=progress_stream,
            )
            continue

        translated = await translate_task
        change_task.cancel()
        await _suppress_cancellation(change_task)
        current_state = _source_state(target.source_path)
        if current_state != initial_state:
            retries += 1
            _raise_if_retry_exhausted(config, retries)
            print(
                f"Source changed before write; retrying ({retries}/{config.max_retranslation_retries})",
                file=progress_stream,
            )
            continue
        _write_text_atomically(target.output_path, translated)
        return current_state


async def _translate_target_once(
    target: DocTranslationTarget,
    target_lang: str,
    config: HotConfig,
    *,
    stream: bool | None,
    template_type: TemplateType,
    template_kwargs: dict[str, object],
) -> str:
    source_text = target.source_path.read_text(encoding="utf-8")
    document_plan = analyze_document_language(source_text, target_lang)
    return await translate_text(
        source_text,
        target_lang,
        config,
        template_type,
        stream=stream,
        document_plan=document_plan,
        terms=template_kwargs.get("terms", ()),
        style=template_kwargs.get("style"),
        background_text=template_kwargs.get("background_text"),
        format_type=template_kwargs.get("format_type"),
        instructions=template_kwargs.get("instructions", ()),
    )


async def _wait_for_path_change(path: Path, initial_state: _SourceState) -> None:
    watchfiles = _load_watchfiles()
    if watchfiles is not None:
        async for changes in watchfiles.awatch(path.parent, recursive=False):
            if (
                _matches_watched_path(path, changes)
                and _source_state(path) != initial_state
            ):
                return
    while True:
        await asyncio.sleep(WATCH_POLL_SECONDS)
        if _source_state(path) != initial_state:
            return


def _matches_watched_path(path: Path, changes: set[tuple[object, str]]) -> bool:
    resolved_path = path.resolve(strict=False)
    for _change, changed_path in changes:
        if Path(changed_path).resolve(strict=False) == resolved_path:
            return True
    return False


def _load_watchfiles() -> ModuleType | None:
    try:
        return import_module("watchfiles")
    except ImportError:
        return None


def _scan_markdown_files(root: Path, *, recursive: bool) -> tuple[Path, ...]:
    files: list[Path] = []
    seen_dirs: set[tuple[int, int]] = set()

    def walk(directory: Path) -> None:
        stat = directory.stat()
        key = (stat.st_dev, stat.st_ino)
        if key in seen_dirs:
            return
        seen_dirs.add(key)
        with os.scandir(directory) as entries:
            for entry in sorted(entries, key=lambda item: item.name):
                path = Path(entry.path)
                if entry.is_symlink() and not path.exists():
                    print(
                        f"Warning: skipping {path.relative_to(root)}: broken symlink",
                        file=sys.stderr,
                    )
                    continue
                if entry.is_dir(follow_symlinks=True):
                    if recursive:
                        walk(path)
                    continue
                if (
                    entry.is_file(follow_symlinks=True)
                    and path.suffix.lower() in MARKDOWN_SUFFIXES
                ):
                    files.append(path.resolve())

    walk(root)
    return tuple(files)


def _file_output_path(
    source_path: Path,
    root: Path,
    output_path: Path | None,
    output_dir: Path | None,
    target_suffix: str,
) -> Path:
    if output_path is not None:
        resolved_output_path = output_path.expanduser().resolve()
        if resolved_output_path == source_path:
            raise ValueError("Output path must differ from source path")
        return resolved_output_path

    target_name = f"{source_path.stem}.{target_suffix}{source_path.suffix}"
    if output_dir is None:
        resolved_output_path = source_path.with_name(target_name).resolve()
        if not resolved_output_path.is_relative_to(root):
            raise ValueError("document output path escapes source root")
        return resolved_output_path
    resolved_output_dir = output_dir.expanduser().resolve()
    relative_source = source_path.relative_to(root)
    resolved_output_path = (
        resolved_output_dir / relative_source.parent / target_name
    ).resolve()
    if not resolved_output_path.is_relative_to(resolved_output_dir):
        raise ValueError("document output path escapes output directory")
    return resolved_output_path


def _validate_markdown_source(path: Path) -> None:
    if path.suffix.lower() not in MARKDOWN_SUFFIXES:
        raise ValueError("translate-doc only supports Markdown sources")


def _target_lang_path_suffix(target_lang: str) -> str:
    suffix = target_lang.strip().lower()
    if not suffix or not all(
        char.isascii() and (char.isalnum() or char == "-") for char in suffix
    ):
        raise ValueError(
            "document target language must contain only ASCII letters, digits, or hyphens"
        )
    return TARGET_SUFFIX_ALIASES.get(suffix, suffix)


def _is_utf8_text_file(path: Path) -> bool:
    try:
        path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return False
    return True


def _source_state(path: Path) -> _SourceState:
    try:
        stat = path.stat()
    except FileNotFoundError:
        return _SourceState(False, 0, 0)
    return _SourceState(True, stat.st_mtime_ns, stat.st_size)


def _write_text_atomically(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            "w",
            encoding="utf-8",
            dir=path.parent,
            prefix=f"{path.name}.",
            suffix=".tmp",
            delete=False,
        ) as handle:
            handle.write(text)
            temp_path = Path(handle.name)
        temp_path.replace(path)
    finally:
        if temp_path is not None and temp_path.exists():
            temp_path.unlink(missing_ok=True)


def _raise_if_retry_exhausted(config: HotConfig, retries: int) -> None:
    if retries > config.max_retranslation_retries:
        raise RuntimeError(
            "Source kept changing during translate-doc; "
            f"exceeded {config.max_retranslation_retries} retries"
        )


async def _suppress_cancellation(task: asyncio.Task[object]) -> None:
    try:
        await task
    except asyncio.CancelledError:
        return
