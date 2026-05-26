from __future__ import annotations

import asyncio
import fcntl
from collections import deque
from contextlib import AsyncExitStack, contextmanager
from collections.abc import Callable, Generator
from dataclasses import dataclass, field
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import sqlite3
import sys
import time
from typing import TextIO, TypeAlias

from hymt.client import TranslationClient
from hymt.config import HotConfig
from hymt.history import DurationEstimate, HistoryDB, TaskRecord, format_duration
from hymt.language import (
    DocumentLanguagePlan,
    DocumentSection,
    build_document_translation_plan,
)
from hymt.segment import Segmenter, create_segmenter
from hymt.templates import TemplateType, build_prompt
from hymt.timing_issue import TimingIssueData, maybe_prompt_timing_issue

JsonValue: TypeAlias = (
    None | bool | int | float | str | list["JsonValue"] | dict[str, "JsonValue"]
)
TokenCallback: TypeAlias = Callable[[str], None]


@contextmanager
def _translation_lock(*, progress: bool = True) -> Generator[None]:
    lock_path = Path.home() / ".cache" / "hymt" / "translate.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    fd = open(lock_path, "w")  # noqa: SIM115
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            if progress:
                print("Waiting for translation lock...", file=sys.stderr)
            fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        fd.close()


def _monotonic() -> float:
    return time.monotonic()


@dataclass(frozen=True)
class TranslationPlan:
    source_tokens: int
    segments: list[str]
    available_source_tokens: int
    _segmenter: Segmenter = field(repr=False, compare=False)
    document_plan: DocumentLanguagePlan | None = None
    segment_section_indexes: tuple[int, ...] = ()
    segment_section_groups: tuple[tuple[int, ...], ...] = ()

    @property
    def segment_count(self) -> int:
        return len(self.segments)

    def count_tokens(self, text: str) -> int:
        return self._segmenter.count_tokens(text)

    def reconstruct(self, translations: list[str]) -> str:
        if self.document_plan is None:
            return "".join(translations)
        if self.segment_section_groups:
            return self._reconstruct_section_groups(translations)
        section_translations: dict[int, list[str]] = {}
        for section_index, translation in zip(
            self.segment_section_indexes, translations, strict=True
        ):
            section_translations.setdefault(section_index, []).append(translation)
        parts: list[str] = []
        for section_index, section in enumerate(self.document_plan.sections):
            if section.should_translate:
                section_output = "".join(section_translations.get(section_index, []))
                if section.text.endswith("\n") and not section_output.endswith("\n"):
                    section_output = f"{section_output}\n"
                parts.append(section_output)
            else:
                parts.append(section.text)
        return "".join(parts)

    def _reconstruct_section_groups(self, translations: list[str]) -> str:
        if self.document_plan is None:
            return "".join(translations)
        group_translations: dict[int, list[str]] = {}
        group_texts: dict[int, str] = {}
        covered_sections: set[int] = set()
        for section_group, translation in zip(
            self.segment_section_groups, translations, strict=True
        ):
            if not section_group:
                continue
            first_section_index = section_group[0]
            group_translations.setdefault(first_section_index, []).append(translation)
            if first_section_index not in group_texts:
                group_texts[first_section_index] = "".join(
                    self.document_plan.sections[index].text for index in section_group
                )
            covered_sections.update(section_group)

        parts: list[str] = []
        for section_index, section in enumerate(self.document_plan.sections):
            if section_index in covered_sections:
                if section_index not in group_translations:
                    continue
                section_output = "".join(group_translations[section_index])
                group_text = group_texts[section_index]
                if group_text.endswith("\n") and not section_output.endswith("\n"):
                    section_output = f"{section_output}\n"
                parts.append(section_output)
            else:
                parts.append(section.text)
        return "".join(parts)


def plan_translation(
    text: str,
    target_lang: str,
    config: HotConfig,
    template_type: TemplateType = TemplateType.DEFAULT,
    *,
    document_plan: DocumentLanguagePlan | None = None,
    **template_kwargs: object,
) -> TranslationPlan:
    segmenter = create_segmenter()
    overhead_prompt = build_prompt("", target_lang, template_type, **template_kwargs)
    prompt_overhead_tokens = segmenter.count_tokens(overhead_prompt)
    available_source_tokens = (
        config.context_window - prompt_overhead_tokens - config.max_output_tokens
    )
    if available_source_tokens <= 0:
        raise ValueError(
            "Config context_window is too small for the selected template and max_output_tokens"
        )

    if not text:
        return TranslationPlan(0, [], available_source_tokens, segmenter)

    active_document_plan = document_plan or build_document_translation_plan(
        text, target_lang
    )
    source_tokens = segmenter.count_tokens(text)
    (
        segments,
        segment_section_indexes,
        segment_section_groups,
    ) = _segment_document_plan(active_document_plan, segmenter, available_source_tokens)
    return TranslationPlan(
        source_tokens,
        segments,
        available_source_tokens,
        segmenter,
        active_document_plan,
        tuple(segment_section_indexes),
        tuple(segment_section_groups),
    )


def _segment_document_plan(
    document_plan: DocumentLanguagePlan,
    segmenter: Segmenter,
    available_source_tokens: int,
) -> tuple[list[str], list[int], list[tuple[int, ...]]]:
    segments: list[str] = []
    segment_section_indexes: list[int] = []
    segment_section_groups: list[tuple[int, ...]] = []
    for section_group in _translation_section_groups(document_plan):
        section_text = "".join(
            document_plan.sections[index].text for index in section_group
        )
        section_segments = segmenter.segment(section_text, available_source_tokens)
        segments.extend(section_segments)
        segment_section_indexes.extend([section_group[0]] * len(section_segments))
        segment_section_groups.extend([section_group] * len(section_segments))
    return segments, segment_section_indexes, segment_section_groups


def _translation_section_groups(
    document_plan: DocumentLanguagePlan,
) -> list[tuple[int, ...]]:
    groups: list[tuple[int, ...]] = []
    current_group: list[int] = []
    sections = document_plan.sections

    for section_index, section in enumerate(sections):
        if section.should_translate:
            current_group.append(section_index)
            continue
        if section.kind == "separator" and current_group:
            next_content_index = _next_content_section_index(sections, section_index)
            if (
                next_content_index is not None
                and sections[next_content_index].should_translate
            ):
                current_group.append(section_index)
                continue
        if current_group:
            groups.append(tuple(current_group))
            current_group = []

    if current_group:
        groups.append(tuple(current_group))
    return groups


def _next_content_section_index(
    sections: tuple[DocumentSection, ...],
    section_index: int,
) -> int | None:
    for next_index in range(section_index + 1, len(sections)):
        section = sections[next_index]
        if section.kind != "separator":
            return next_index
    return None


async def translate_text(
    text: str,
    target_lang: str,
    config: HotConfig,
    template_type: TemplateType = TemplateType.DEFAULT,
    *,
    stream: bool | None = None,
    progress: bool | None = None,
    on_token: TokenCallback | None = None,
    document_plan: DocumentLanguagePlan | None = None,
    **template_kwargs: object,
) -> str:
    if not text:
        return ""

    template_name = template_type.value
    input_hash = _translation_cache_hash(
        text, target_lang, template_type, template_kwargs
    )
    plan = plan_translation(
        text,
        target_lang,
        config,
        template_type,
        document_plan=document_plan,
        **template_kwargs,
    )
    progress_enabled = _progress_enabled(sys.stderr, progress)
    if progress_enabled:
        print(
            f"Source tokens: {plan.source_tokens}; segments: {plan.segment_count}",
            file=sys.stderr,
        )
    history = HistoryDB()
    cv = config.config_version
    initial_estimate = history.estimate(
        plan.segment_count,
        config.concurrency,
        target_lang,
        template_name,
        config_version=cv,
    )
    if progress_enabled and initial_estimate is not None:
        _print_estimate(initial_estimate)
    stream_enabled = _stream_enabled(config, stream)
    options_hash = _template_options_hash(template_kwargs)
    segment_hashes = [_segment_cache_hash(segment) for segment in plan.segments]
    segment_tokens = [plan.count_tokens(segment) for segment in plan.segments]
    translations: list[str | None] = [None] * plan.segment_count

    with _translation_lock(progress=progress_enabled):
        started_at = datetime.now(timezone.utc)
        started_monotonic = _monotonic()
        progress_reporter = TranslationProgress(
            plan.segment_count,
            config.concurrency,
            started_monotonic,
            sys.stderr,
            enabled=progress_enabled,
        )
        completed = 0
        completed_tokens = 0
        missing_indexes: list[int] = []
        try:
            if stream_enabled and on_token is not None:
                completed, completed_tokens = await _translate_streaming_plan(
                    history,
                    plan,
                    segment_hashes,
                    segment_tokens,
                    translations,
                    config,
                    target_lang,
                    template_type,
                    template_kwargs,
                    options_hash,
                    template_name,
                    progress_reporter,
                    completed,
                    completed_tokens,
                    on_token,
                )
            else:
                for index, (segment_hash, token_count) in enumerate(
                    zip(segment_hashes, segment_tokens, strict=True)
                ):
                    cached = history.find_segment_cached(
                        segment_hash, target_lang, template_name, options_hash
                    )
                    if cached is None:
                        missing_indexes.append(index)
                        continue
                    translations[index] = cached
                    completed += 1
                    completed_tokens += token_count
                    progress_reporter.update(completed, completed_tokens, 0.0)

            if missing_indexes:
                async with TranslationClient(config) as client:
                    completed, completed_tokens = await _translate_missing_segments(
                        client,
                        history,
                        plan,
                        missing_indexes,
                        segment_hashes,
                        segment_tokens,
                        translations,
                        target_lang,
                        template_type,
                        template_kwargs,
                        options_hash,
                        template_name,
                        progress_reporter,
                        completed,
                        completed_tokens,
                        config.concurrency,
                    )
        finally:
            progress_reporter.finish()

    translated = plan.reconstruct(_completed_translations(translations))
    finished_at = datetime.now(timezone.utc)
    duration_seconds = _monotonic() - started_monotonic
    output_tokens = plan.count_tokens(translated)
    tokens_per_second = (
        output_tokens / duration_seconds if duration_seconds > 0 else 0.0
    )
    _record_successful_translation(
        history,
        TaskRecord(
            started_at=started_at.isoformat(timespec="seconds"),
            finished_at=finished_at.isoformat(timespec="seconds"),
            duration_seconds=duration_seconds,
            input_tokens=plan.source_tokens,
            output_tokens=output_tokens,
            segments=plan.segment_count,
            concurrency=config.concurrency,
            source_lang=None,
            target_lang=target_lang,
            template_type=template_name,
            model=config.model or None,
            tokens_per_second=tokens_per_second,
            input_chars=len(text),
            output_chars=len(translated),
            output_text=translated,
            input_hash=input_hash,
            config_version=cv,
        ),
        progress=progress_enabled,
    )
    maybe_prompt_timing_issue(
        history,
        initial_estimate,
        TimingIssueData(
            input_tokens=plan.source_tokens,
            output_tokens=output_tokens,
            segments=plan.segment_count,
            actual_seconds=duration_seconds,
            estimated_seconds=initial_estimate.seconds if initial_estimate else 0.0,
            config_version=cv,
            target_lang=target_lang,
            template_type=template_name,
            concurrency=config.concurrency,
            model=config.model or None,
        ),
        getattr(config, "timing_divergence_threshold", 2.0),
    )
    return translated


async def translate_file(
    input_path: Path,
    output_path: Path | None,
    target_lang: str,
    config: HotConfig,
    template_type: TemplateType = TemplateType.DEFAULT,
    *,
    stream: bool | None = None,
    progress: bool | None = None,
    source_text: str | None = None,
    document_plan: DocumentLanguagePlan | None = None,
    **template_kwargs: object,
) -> None:
    text = (
        source_text
        if source_text is not None
        else input_path.read_text(encoding="utf-8")
    )
    stream_enabled = _stream_enabled(config, stream)
    streamed_chars = 0

    def write_token(token: str) -> None:
        nonlocal streamed_chars
        streamed_chars += len(token)
        sys.stdout.write(token)
        sys.stdout.flush()

    translated = await translate_text(
        text,
        target_lang,
        config,
        template_type,
        stream=stream_enabled,
        progress=progress,
        on_token=write_token if output_path is None and stream_enabled else None,
        document_plan=document_plan,
        **template_kwargs,
    )
    if output_path is None:
        if not stream_enabled or streamed_chars == 0:
            sys.stdout.write(translated)
        if not translated.endswith("\n"):
            sys.stdout.write("\n")
        sys.stdout.flush()
        return
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(translated, encoding="utf-8")


class TranslationProgress:
    def __init__(
        self,
        total_segments: int,
        concurrency: int,
        started_monotonic: float,
        stream: TextIO,
        *,
        enabled: bool = True,
    ) -> None:
        self._total_segments = total_segments
        self._concurrency = max(1, concurrency)
        self._started_monotonic = started_monotonic
        self._stream = stream
        self._enabled = enabled
        self._recent_segment_seconds: deque[float] = deque(maxlen=5)
        self._uses_carriage_return = stream.isatty()
        self._printed = False

    def update(
        self, completed_segments: int, completed_tokens: int, segment_seconds: float
    ) -> None:
        if not self._enabled:
            return
        if self._total_segments == 0:
            return
        self._recent_segment_seconds.append(max(0.0, segment_seconds))
        elapsed = _monotonic() - self._started_monotonic
        remaining_segments = max(0, self._total_segments - completed_segments)
        eta_seconds = self._estimate_remaining_seconds(remaining_segments)
        tokens_per_second = completed_tokens / elapsed if elapsed > 0 else 0.0
        percent = completed_segments / self._total_segments * 100
        line = (
            f"[{completed_segments}/{self._total_segments}] {percent:.2f}% | "
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
        if not self._enabled:
            return
        if self._printed and self._uses_carriage_return:
            self._stream.write("\n")
            self._stream.flush()

    def _estimate_remaining_seconds(self, remaining_segments: int) -> float:
        if remaining_segments == 0 or not self._recent_segment_seconds:
            return 0.0
        average_seconds = sum(self._recent_segment_seconds) / len(
            self._recent_segment_seconds
        )
        effective_concurrency = min(self._concurrency, max(1, remaining_segments))
        return average_seconds * remaining_segments / effective_concurrency


async def _translate_streaming_plan(
    history: HistoryDB,
    plan: TranslationPlan,
    segment_hashes: list[str],
    segment_tokens: list[int],
    translations: list[str | None],
    config: HotConfig,
    target_lang: str,
    template_type: TemplateType,
    template_kwargs: dict[str, object],
    options_hash: str,
    template_name: str,
    progress: TranslationProgress,
    completed: int,
    completed_tokens: int,
    on_token: TokenCallback,
) -> tuple[int, int]:
    async with AsyncExitStack() as stack:
        client: TranslationClient | None = None

        async def translate_index(index: int) -> None:
            nonlocal client, completed, completed_tokens
            cached = history.find_segment_cached(
                segment_hashes[index], target_lang, template_name, options_hash
            )
            if cached is None:
                if client is None:
                    client = await stack.enter_async_context(TranslationClient(config))
                segment_started = _monotonic()
                translated_segment = await _translate_streaming_segment(
                    client,
                    build_prompt(
                        plan.segments[index],
                        target_lang,
                        template_type,
                        **template_kwargs,
                    ),
                    on_token,
                )
                segment_seconds = _monotonic() - segment_started
                history.store_segment_cache(
                    segment_hashes[index],
                    target_lang,
                    template_name,
                    translated_segment,
                    datetime.now(timezone.utc).isoformat(timespec="seconds"),
                    options_hash=options_hash,
                )
            else:
                translated_segment = cached
                segment_seconds = 0.0
                on_token(cached)
            translations[index] = translated_segment
            completed += 1
            completed_tokens += segment_tokens[index]
            progress.update(completed, completed_tokens, segment_seconds)

        if plan.document_plan is None:
            for index in range(plan.segment_count):
                await translate_index(index)
            return completed, completed_tokens

        if plan.segment_section_groups:
            (
                group_segment_indexes,
                covered_sections,
                group_texts,
            ) = _streaming_section_groups(plan)
            for section_index, section in enumerate(plan.document_plan.sections):
                if section_index not in covered_sections:
                    on_token(section.text)
                    continue
                if section_index not in group_segment_indexes:
                    continue

                segment_indexes = group_segment_indexes[section_index]
                for index in segment_indexes:
                    await translate_index(index)
                group_output = "".join(
                    str(translations[index])
                    for index in segment_indexes
                    if translations[index] is not None
                )
                group_text = group_texts[section_index]
                if group_text.endswith("\n") and not group_output.endswith("\n"):
                    on_token("\n")
                    last_index = segment_indexes[-1]
                    translations[last_index] = f"{translations[last_index]}\n"
            return completed, completed_tokens

        segment_index = 0
        for section_index, section in enumerate(plan.document_plan.sections):
            if not section.should_translate:
                on_token(section.text)
                continue
            section_start = segment_index
            while (
                segment_index < plan.segment_count
                and plan.segment_section_indexes[segment_index] == section_index
            ):
                await translate_index(segment_index)
                segment_index += 1
            section_output = "".join(
                str(translations[index])
                for index in range(section_start, segment_index)
                if translations[index] is not None
            )
            if section.text.endswith("\n") and not section_output.endswith("\n"):
                on_token("\n")
                if segment_index > section_start:
                    last_index = segment_index - 1
                    translations[last_index] = f"{translations[last_index]}\n"
        while segment_index < plan.segment_count:
            await translate_index(segment_index)
            segment_index += 1
        return completed, completed_tokens


def _streaming_section_groups(
    plan: TranslationPlan,
) -> tuple[dict[int, list[int]], set[int], dict[int, str]]:
    if plan.document_plan is None:
        return {}, set(), {}

    group_segment_indexes: dict[int, list[int]] = {}
    covered_sections: set[int] = set()
    group_texts: dict[int, str] = {}
    for segment_index, section_group in enumerate(plan.segment_section_groups):
        if not section_group:
            continue
        first_section_index = section_group[0]
        group_segment_indexes.setdefault(first_section_index, []).append(segment_index)
        if first_section_index not in group_texts:
            group_texts[first_section_index] = "".join(
                plan.document_plan.sections[index].text for index in section_group
            )
        covered_sections.update(section_group)
    return group_segment_indexes, covered_sections, group_texts


async def _translate_streaming_segment(
    client: TranslationClient, prompt: str, on_token: TokenCallback
) -> str:
    parts: list[str] = []
    async for token in client.translate_stream(prompt):
        parts.append(token)
        on_token(token)
    return "".join(parts)


async def _translate_missing_segments(
    client: TranslationClient,
    history: HistoryDB,
    plan: TranslationPlan,
    missing_indexes: list[int],
    segment_hashes: list[str],
    segment_tokens: list[int],
    translations: list[str | None],
    target_lang: str,
    template_type: TemplateType,
    template_kwargs: dict[str, object],
    options_hash: str,
    template_name: str,
    progress: TranslationProgress,
    completed: int,
    completed_tokens: int,
    concurrency: int,
) -> tuple[int, int]:
    semaphore = asyncio.Semaphore(max(1, concurrency))
    progress_lock = asyncio.Lock()

    async def run(index: int) -> None:
        nonlocal completed, completed_tokens
        prompt = build_prompt(
            plan.segments[index], target_lang, template_type, **template_kwargs
        )
        async with semaphore:
            segment_started = _monotonic()
            translated_segment = await client.translate(prompt)
            segment_seconds = _monotonic() - segment_started
        translations[index] = translated_segment
        history.store_segment_cache(
            segment_hashes[index],
            target_lang,
            template_name,
            translated_segment,
            datetime.now(timezone.utc).isoformat(timespec="seconds"),
            options_hash=options_hash,
        )
        async with progress_lock:
            completed += 1
            completed_tokens += segment_tokens[index]
            progress.update(completed, completed_tokens, segment_seconds)

    await asyncio.gather(*(run(index) for index in missing_indexes))
    return completed, completed_tokens


def _completed_translations(translations: list[str | None]) -> list[str]:
    missing = [
        index for index, translation in enumerate(translations) if translation is None
    ]
    if missing:
        raise RuntimeError(f"Missing translated segments: {missing}")
    return [str(translation) for translation in translations]


def _stream_enabled(config: HotConfig, override: bool | None) -> bool:
    if override is not None:
        return override
    value = getattr(config, "stream", True)
    return value if isinstance(value, bool) else True


def _progress_enabled(stream: TextIO, override: bool | None) -> bool:
    if override is not None:
        return override
    return stream.isatty()


def _print_estimate(est: DurationEstimate) -> None:
    stats = est.stats
    if len(est.versions_used) <= 1:
        print(
            f"Estimated time: ~{format_duration(est.seconds)} "
            f"based on {stats.count} historical tasks",
            file=sys.stderr,
        )
        return
    slow_tps = max(0.1, stats.p5_tokens_per_second)
    slow_seconds = est.estimated_output_tokens / slow_tps / max(1, est.concurrency)
    lo = format_duration(est.seconds)
    hi = format_duration(slow_seconds)
    vers = ",".join(str(v) for v in est.versions_used)
    print(
        f"Estimated time: ~{lo}–{hi} based on {stats.count} tasks (versions {vers})",
        file=sys.stderr,
    )


def _record_successful_translation(
    history: HistoryDB, record: TaskRecord, *, progress: bool = True
) -> None:
    try:
        history.insert_task(record)
    except (OSError, sqlite3.Error) as exc:
        print(f"Warning: failed to record timing history: {exc}", file=sys.stderr)
        return
    if not progress:
        return
    print(
        f"Completed in {format_duration(record.duration_seconds)} | "
        f"avg {record.tokens_per_second:.1f} tok/s | timing recorded",
        file=sys.stderr,
    )


def _translation_cache_hash(
    text: str,
    target_lang: str,
    template_type: TemplateType,
    template_kwargs: dict[str, object],
) -> str:
    payload: dict[str, JsonValue] = {
        "source_text": text,
        "target_lang": target_lang,
        "template_type": template_type.value,
        "template_kwargs": _normalize_cache_kwargs(template_kwargs),
    }
    encoded = json.dumps(
        payload, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    )
    return hashlib.sha256(encoded.encode("utf-8")).hexdigest()


def _segment_cache_hash(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _template_options_hash(template_kwargs: dict[str, object]) -> str:
    normalized = _normalize_cache_kwargs(template_kwargs)
    if not normalized:
        return ""
    encoded = json.dumps(
        normalized, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    )
    return hashlib.sha256(encoded.encode("utf-8")).hexdigest()


def _normalize_cache_kwargs(template_kwargs: dict[str, object]) -> dict[str, JsonValue]:
    return {
        key: _normalize_cache_value(template_kwargs[key])
        for key in sorted(template_kwargs)
    }


def _normalize_cache_value(value: object) -> JsonValue:
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, (tuple, list)):
        return [_normalize_cache_value(item) for item in value]
    if isinstance(value, dict):
        return {
            str(key): _normalize_cache_value(value[key])
            for key in sorted(value, key=lambda item: str(item))
        }
    return str(value)
