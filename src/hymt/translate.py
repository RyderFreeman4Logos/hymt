from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
import sqlite3
import sys
import time

from hymt.client import TranslationClient
from hymt.config import HotConfig
from hymt.history import HistoryDB, TaskRecord, format_duration
from hymt.segment import Segmenter, ensure_tokenizer
from hymt.templates import TemplateType, build_prompt


@dataclass(frozen=True)
class TranslationPlan:
    source_tokens: int
    segments: list[str]
    available_source_tokens: int
    _segmenter: Segmenter = field(repr=False, compare=False)

    @property
    def segment_count(self) -> int:
        return len(self.segments)

    def count_tokens(self, text: str) -> int:
        return self._segmenter.count_tokens(text)


def plan_translation(
    text: str,
    target_lang: str,
    config: HotConfig,
    template_type: TemplateType = TemplateType.DEFAULT,
    **template_kwargs: object,
) -> TranslationPlan:
    tokenizer_path = ensure_tokenizer()
    segmenter = Segmenter(tokenizer_path)
    overhead_prompt = build_prompt("", target_lang, template_type, **template_kwargs)
    prompt_overhead_tokens = segmenter.count_tokens(overhead_prompt)
    available_source_tokens = config.context_window - prompt_overhead_tokens - config.max_output_tokens
    if available_source_tokens <= 0:
        raise ValueError(
            "Config context_window is too small for the selected template and max_output_tokens"
        )

    if not text:
        return TranslationPlan(0, [], available_source_tokens, segmenter)

    source_tokens = segmenter.count_tokens(text)
    segments = segmenter.segment(text, available_source_tokens)
    return TranslationPlan(source_tokens, segments, available_source_tokens, segmenter)


async def translate_text(
    text: str,
    target_lang: str,
    config: HotConfig,
    template_type: TemplateType = TemplateType.DEFAULT,
    **template_kwargs: object,
) -> str:
    if not text:
        return ""

    started_at = datetime.now(timezone.utc)
    started_monotonic = time.monotonic()
    plan = plan_translation(text, target_lang, config, template_type, **template_kwargs)
    print(f"Source tokens: {plan.source_tokens}; segments: {plan.segment_count}", file=sys.stderr)
    history = HistoryDB()
    initial_estimate = history.estimate(
        plan.segment_count,
        config.concurrency,
        target_lang,
        template_type.value,
    )
    if initial_estimate is not None:
        print(
            "Estimated time: "
            f"~{format_duration(initial_estimate.seconds)} "
            f"based on {initial_estimate.stats.count} historical tasks",
            file=sys.stderr,
        )
    prompts = [
        build_prompt(segment, target_lang, template_type, **template_kwargs)
        for segment in plan.segments
    ]

    def report_progress(done: int, total: int) -> None:
        if total > 1:
            elapsed = time.monotonic() - started_monotonic
            percent = int(done / total * 100)
            eta_seconds = elapsed / done * (total - done) if done else 0.0
            processed_tokens = plan.source_tokens * done / total
            tokens_per_second = processed_tokens / elapsed if elapsed > 0 else 0.0
            print(
                f"[{done}/{total}] {percent}% | "
                f"elapsed {format_duration(elapsed)} | "
                f"eta {format_duration(eta_seconds)} | "
                f"{tokens_per_second:.1f} tok/s",
                file=sys.stderr,
            )

    async with TranslationClient(config) as client:
        translations = await client.translate_batch(prompts, on_progress=report_progress)
    translated = "".join(translations)
    finished_at = datetime.now(timezone.utc)
    duration_seconds = time.monotonic() - started_monotonic
    output_tokens = plan.count_tokens(translated)
    tokens_per_second = output_tokens / duration_seconds if duration_seconds > 0 else 0.0
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
            template_type=template_type.value,
            model=config.model or None,
            tokens_per_second=tokens_per_second,
            input_chars=len(text),
            output_chars=len(translated),
        ),
    )
    return translated


async def translate_file(
    input_path: Path,
    output_path: Path | None,
    target_lang: str,
    config: HotConfig,
    template_type: TemplateType = TemplateType.DEFAULT,
    **template_kwargs: object,
) -> None:
    text = input_path.read_text(encoding="utf-8")
    translated = await translate_text(text, target_lang, config, template_type, **template_kwargs)
    if output_path is None:
        sys.stdout.write(translated)
        return
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(translated, encoding="utf-8")


def _record_successful_translation(history: HistoryDB, record: TaskRecord) -> None:
    try:
        history.insert_task(record)
    except (OSError, sqlite3.Error) as exc:
        print(f"Warning: failed to record timing history: {exc}", file=sys.stderr)
        return
    print(
        f"Completed in {format_duration(record.duration_seconds)} | "
        f"avg {record.tokens_per_second:.1f} tok/s | timing recorded",
        file=sys.stderr,
    )
