from __future__ import annotations

from collections import deque
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
import hashlib
import os
from pathlib import Path
import sys
import time
from typing import Final, TextIO

from hymt.config import HotConfig
from hymt.history import HistoryDB, format_duration
from hymt.language import DocumentLanguagePlan, analyze_document_language
from hymt.templates import TemplateType
from hymt.translate import (
    _segment_cache_hash,
    _template_options_hash,
    plan_translation,
    translate_text,
)


__all__ = [
    "BatchFilePlan",
    "BatchPlan",
    "BatchSkippedFile",
    "build_batch_plan",
    "run_batch_translation",
    "show_batch_preview",
]


TEXT_FILE_SUFFIXES: Final = frozenset({".md", ".txt"})


class _BatchOutputPathEscapeError(ValueError):
    def __init__(self, output_path: Path, reason: str) -> None:
        super().__init__(f"batch output path escapes boundary: {output_path}")
        self.reason = reason


@dataclass(frozen=True)
class BatchSourceFile:
    path: Path
    text: str
    file_hash: str


@dataclass(frozen=True)
class BatchSkippedFile:
    path: Path
    relative_path: Path
    reason: str


@dataclass(frozen=True)
class BatchFilePlan:
    source_path: Path
    relative_path: Path
    output_path: Path
    text: str
    file_hash: str
    document_plan: DocumentLanguagePlan
    source_tokens: int
    segment_hashes: tuple[str, ...]
    cached_segments: int
    estimated_seconds: float | None

    @property
    def segment_count(self) -> int:
        return len(self.segment_hashes)

    @property
    def missing_segments(self) -> int:
        return max(0, self.segment_count - self.cached_segments)

    @property
    def cache_status(self) -> str:
        if self.segment_count == 0 or self.cached_segments == self.segment_count:
            return "full"
        if self.cached_segments == 0:
            return "none"
        return "partial"


@dataclass(frozen=True)
class BatchPlan:
    root: Path
    files: tuple[BatchFilePlan, ...]
    skipped: tuple[BatchSkippedFile, ...]

    @property
    def total_source_tokens(self) -> int:
        return sum(file.source_tokens for file in self.files)

    @property
    def total_segments(self) -> int:
        return sum(file.segment_count for file in self.files)

    @property
    def total_cached_segments(self) -> int:
        return sum(file.cached_segments for file in self.files)

    @property
    def total_missing_segments(self) -> int:
        return sum(file.missing_segments for file in self.files)

    @property
    def total_estimated_seconds(self) -> float | None:
        total = 0.0
        for file in self.files:
            if file.estimated_seconds is None and file.missing_segments > 0:
                return None
            total += file.estimated_seconds or 0.0
        return total


def build_batch_plan(
    directory: Path,
    output_dir: Path | None,
    target_lang: str,
    config: HotConfig,
    template_type: TemplateType,
    template_kwargs: dict[str, object],
    *,
    recursive: bool = False,
    history: HistoryDB | None = None,
    progress_stream: TextIO | None = None,
) -> BatchPlan:
    resolved_root = directory.expanduser().resolve(strict=True)
    progress = _BatchPlanningProgress(progress_stream)
    source_paths = _scan_text_files(resolved_root, recursive=recursive)
    progress.scanned(len(source_paths))
    sources = _read_sources_parallel(source_paths, root=resolved_root)
    db = history or HistoryDB()
    template_name = template_type.value
    options_hash = _template_options_hash(template_kwargs)

    files: list[BatchFilePlan] = []
    skipped: list[BatchSkippedFile] = []
    for index, source in enumerate(sources, start=1):
        relative_path = _relative_to_root(source.path, resolved_root)
        progress.analyzing(index, len(source_paths), relative_path)
        try:
            output_path = _output_path(
                source.path,
                resolved_root,
                output_dir,
                target_lang,
            )
        except _BatchOutputPathEscapeError as error:
            skipped.append(BatchSkippedFile(source.path, relative_path, error.reason))
            print(
                f"Warning: skipping {relative_path}: {error.reason}",
                file=sys.stderr,
            )
            continue
        document_plan = analyze_document_language(source.text, target_lang)
        if _is_target_language_document(document_plan):
            skipped.append(
                BatchSkippedFile(source.path, relative_path, "already target language")
            )
            continue

        translation_plan = plan_translation(
            source.text,
            target_lang,
            config,
            template_type,
            document_plan=document_plan,
            **template_kwargs,
        )
        segment_hashes = tuple(
            _segment_cache_hash(segment) for segment in translation_plan.segments
        )
        cached_hashes = db.find_cached_segment_hashes(
            segment_hashes,
            target_lang,
            template_name,
            options_hash,
        )
        cached_segments = sum(1 for value in segment_hashes if value in cached_hashes)
        missing_segments = max(0, len(segment_hashes) - cached_segments)
        estimate = _estimate_file_seconds(
            db,
            missing_segments,
            config,
            target_lang,
            template_name,
        )
        files.append(
            BatchFilePlan(
                source_path=source.path,
                relative_path=relative_path,
                output_path=output_path,
                text=source.text,
                file_hash=source.file_hash,
                document_plan=document_plan,
                source_tokens=translation_plan.source_tokens,
                segment_hashes=segment_hashes,
                cached_segments=cached_segments,
                estimated_seconds=estimate,
            )
        )

    progress.complete(selected=len(files), skipped=len(skipped))
    return BatchPlan(
        root=resolved_root,
        files=tuple(files),
        skipped=tuple(skipped),
    )


def show_batch_preview(plan: BatchPlan, stream: TextIO = sys.stderr) -> None:
    print(f"Batch root: {plan.root}", file=stream)
    print(
        f"Files: {len(plan.files)} selected, {len(plan.skipped)} skipped | "
        f"segments {plan.total_cached_segments}/{plan.total_segments} cached",
        file=stream,
    )
    for index, file in enumerate(plan.files, start=1):
        estimate = _format_estimate(file.estimated_seconds, file.missing_segments)
        print(
            f"[{index}/{len(plan.files)}] {file.relative_path} | "
            f"cache={file.cache_status} "
            f"({file.cached_segments}/{file.segment_count}) | "
            f"eta {estimate} | output {file.output_path}",
            file=stream,
        )
    if plan.skipped:
        print("Skipped:", file=stream)
        for skipped in plan.skipped:
            print(
                f"  {skipped.relative_path} | {skipped.reason}",
                file=stream,
            )
    total = plan.total_estimated_seconds
    print(f"Total estimated time: {_format_total_estimate(total)}", file=stream)


async def run_batch_translation(
    plan: BatchPlan,
    target_lang: str,
    config: HotConfig,
    template_type: TemplateType,
    *,
    stream: bool | None = None,
    template_kwargs: dict[str, object] | None = None,
    progress_stream: TextIO = sys.stderr,
) -> None:
    kwargs = template_kwargs or {}
    progress = BatchProgress(
        len(plan.files),
        time.monotonic(),
        progress_stream,
    )
    completed_tokens = 0
    try:
        for index, file in enumerate(plan.files, start=1):
            print(
                f"Batch file {index}/{len(plan.files)}: {file.relative_path}",
                file=progress_stream,
            )
            started = time.monotonic()
            translated = await translate_text(
                file.text,
                target_lang,
                config,
                template_type,
                stream=stream,
                document_plan=file.document_plan,
                terms=kwargs.get("terms", ()),
                style=kwargs.get("style"),
                background_text=kwargs.get("background_text"),
                format_type=kwargs.get("format_type"),
                instructions=kwargs.get("instructions", ()),
            )
            file.output_path.parent.mkdir(parents=True, exist_ok=True)
            file.output_path.write_text(translated, encoding="utf-8")
            completed_tokens += file.source_tokens
            progress.update(index, completed_tokens, time.monotonic() - started)
    finally:
        progress.finish()


class BatchProgress:
    def __init__(
        self,
        total_files: int,
        started_monotonic: float,
        stream: TextIO,
    ) -> None:
        self._total_files = total_files
        self._started_monotonic = started_monotonic
        self._stream = stream
        self._recent_file_seconds: deque[float] = deque(maxlen=5)
        self._uses_carriage_return = stream.isatty()
        self._printed = False

    def update(
        self, completed_files: int, completed_tokens: int, file_seconds: float
    ) -> None:
        if self._total_files == 0:
            return
        self._recent_file_seconds.append(max(0.0, file_seconds))
        elapsed = time.monotonic() - self._started_monotonic
        remaining_files = max(0, self._total_files - completed_files)
        eta_seconds = self._estimate_remaining_seconds(remaining_files)
        tokens_per_second = completed_tokens / elapsed if elapsed > 0 else 0.0
        percent = completed_files / self._total_files * 100
        line = (
            f"[{completed_files}/{self._total_files}] {percent:.2f}% | "
            f"elapsed {format_duration(elapsed)} | "
            f"eta {format_duration(eta_seconds)} | "
            f"{tokens_per_second:.2f} tok/s"
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

    def _estimate_remaining_seconds(self, remaining_files: int) -> float:
        if remaining_files == 0 or not self._recent_file_seconds:
            return 0.0
        average_seconds = sum(self._recent_file_seconds) / len(
            self._recent_file_seconds
        )
        return average_seconds * remaining_files


class _BatchPlanningProgress:
    def __init__(self, stream: TextIO | None) -> None:
        self._stream = stream
        self._uses_carriage_return = stream.isatty() if stream is not None else False
        self._printed = False

    def scanned(self, file_count: int) -> None:
        self._write(f"Batch planning: scanned {file_count} file(s)")

    def analyzing(self, index: int, total: int, relative_path: Path) -> None:
        self._write(f"Batch planning: analyzing [{index}/{total}] {relative_path}")

    def complete(self, *, selected: int, skipped: int) -> None:
        self._write(f"Batch planning complete: {selected} selected, {skipped} skipped")
        self._finish()

    def _write(self, line: str) -> None:
        if self._stream is None:
            return
        if self._uses_carriage_return:
            self._stream.write(f"\r{line}\033[K\r")
        else:
            self._stream.write(f"{line}\n")
        self._stream.flush()
        self._printed = True

    def _finish(self) -> None:
        if self._printed and self._uses_carriage_return and self._stream is not None:
            self._stream.write("\n")
            self._stream.flush()


def _scan_text_files(root: Path, *, recursive: bool) -> tuple[Path, ...]:
    if not root.is_dir():
        raise NotADirectoryError(str(root))

    files: list[Path] = []
    seen_dirs: set[tuple[int, int]] = set()

    def walk(directory: Path) -> None:
        stat = directory.stat()
        key = (stat.st_dev, stat.st_ino)
        if key in seen_dirs:
            return
        seen_dirs.add(key)
        with os.scandir(directory) as entries:
            sorted_entries = sorted(entries, key=lambda entry: entry.name)
            for entry in sorted_entries:
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
                    and path.suffix.lower() in TEXT_FILE_SUFFIXES
                ):
                    files.append(path)

    walk(root)
    return tuple(files)


def _read_sources_parallel(
    paths: tuple[Path, ...], *, root: Path
) -> tuple[BatchSourceFile, ...]:
    if not paths:
        return ()
    with ThreadPoolExecutor() as executor:
        sources: list[BatchSourceFile] = []
        for path, source in zip(paths, executor.map(_read_source_file, paths)):
            if source is None:
                print(
                    f"Warning: skipping {_relative_to_root(path, root)}: not valid UTF-8",
                    file=sys.stderr,
                )
                continue
            sources.append(source)
        return tuple(sources)


def _read_source_file(path: Path) -> BatchSourceFile | None:
    try:
        raw = path.read_bytes()
        text = raw.decode("utf-8")
    except (OSError, UnicodeDecodeError):
        return None
    return BatchSourceFile(
        path=path,
        text=text,
        file_hash=hashlib.sha256(raw).hexdigest(),
    )


def _is_target_language_document(plan: DocumentLanguagePlan) -> bool:
    return (
        plan.paragraph_count > 0 and plan.target_paragraph_count == plan.paragraph_count
    )


def _output_path(
    source_path: Path,
    root: Path,
    output_dir: Path | None,
    target_lang: str,
) -> Path:
    target_suffix = _target_lang_path_suffix(target_lang)
    target_name = f"{source_path.stem}.{target_suffix}{source_path.suffix}"
    if output_dir is None:
        output_path = source_path.with_name(target_name)
        resolved_output_path = output_path.resolve()
        if not resolved_output_path.is_relative_to(root):
            raise _BatchOutputPathEscapeError(
                output_path,
                "output path escapes scan root",
            )
        return resolved_output_path
    resolved_output_dir = output_dir.expanduser().resolve()
    output_path = (
        resolved_output_dir / _relative_to_root(source_path, root).parent / target_name
    )
    resolved_output_path = output_path.resolve()
    if not resolved_output_path.is_relative_to(resolved_output_dir):
        raise _BatchOutputPathEscapeError(
            output_path,
            "output path escapes output directory",
        )
    return resolved_output_path


def _target_lang_path_suffix(target_lang: str) -> str:
    suffix = target_lang.strip()
    if not suffix or not all(
        char.isascii() and (char.isalnum() or char == "-") for char in suffix
    ):
        raise ValueError(
            "batch target language must contain only ASCII letters, digits, or hyphens"
        )
    return suffix


def _relative_to_root(path: Path, root: Path) -> Path:
    try:
        return path.relative_to(root)
    except ValueError:
        return Path(path.name)


def _estimate_file_seconds(
    history: HistoryDB,
    missing_segments: int,
    config: HotConfig,
    target_lang: str,
    template_name: str,
) -> float | None:
    if missing_segments == 0:
        return 0.0
    estimate = history.estimate(
        missing_segments,
        config.concurrency,
        target_lang,
        template_name,
        config_version=config.config_version,
    )
    return estimate.seconds if estimate is not None else None


def _format_estimate(seconds: float | None, missing_segments: int) -> str:
    if missing_segments == 0:
        return format_duration(0)
    if seconds is None:
        return "unknown"
    return format_duration(seconds)


def _format_total_estimate(seconds: float | None) -> str:
    return "unknown" if seconds is None else format_duration(seconds)
