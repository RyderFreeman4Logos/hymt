from __future__ import annotations

from dataclasses import dataclass

__all__ = [
    "CompletenessResult",
    "CompletenessStats",
    "CompletenessThresholds",
    "validate_completeness",
]


@dataclass(frozen=True)
class CompletenessThresholds:
    zh_to_en_min_ratio: float = 0.3
    en_to_zh_min_ratio: float = 0.4
    min_paragraph_ratio: float = 0.5


@dataclass(frozen=True)
class CompletenessStats:
    char_count: int
    paragraph_count: int
    heading_count: int


@dataclass(frozen=True)
class CompletenessResult:
    is_complete: bool
    checks_failed: list[str]
    input_stats: CompletenessStats
    output_stats: CompletenessStats


def validate_completeness(
    input_text: str,
    output_text: str,
    target_lang: str,
    thresholds: CompletenessThresholds | None = None,
) -> CompletenessResult:
    active_thresholds = thresholds or CompletenessThresholds()
    input_stats = _stats(input_text)
    output_stats = _stats(output_text)
    checks_failed: list[str] = []

    min_ratio = _min_char_ratio(target_lang, active_thresholds)
    if min_ratio is not None and input_stats.char_count > 0:
        actual_ratio = output_stats.char_count / input_stats.char_count
        if actual_ratio < min_ratio:
            checks_failed.append("token_ratio")

    if input_stats.paragraph_count > 0:
        paragraph_ratio = output_stats.paragraph_count / input_stats.paragraph_count
        if paragraph_ratio < active_thresholds.min_paragraph_ratio:
            checks_failed.append("paragraph_count")

    if (
        input_stats.heading_count > 0
        and output_stats.heading_count < input_stats.heading_count
    ):
        checks_failed.append("heading_preservation")

    return CompletenessResult(
        is_complete=not checks_failed,
        checks_failed=checks_failed,
        input_stats=input_stats,
        output_stats=output_stats,
    )


def _stats(text: str) -> CompletenessStats:
    return CompletenessStats(
        char_count=len(text),
        paragraph_count=_count_paragraphs(text),
        heading_count=_count_markdown_headings(text),
    )


def _count_paragraphs(text: str) -> int:
    return len([block for block in text.split("\n\n") if block.strip()])


def _count_markdown_headings(text: str) -> int:
    return sum(1 for line in text.splitlines() if line.startswith("#"))


def _min_char_ratio(
    target_lang: str, thresholds: CompletenessThresholds
) -> float | None:
    normalized = target_lang.lower().strip().replace("_", "-")
    if normalized == "en" or normalized.startswith("en-"):
        return thresholds.zh_to_en_min_ratio
    if normalized == "zh" or normalized.startswith("zh-"):
        return thresholds.en_to_zh_min_ratio
    return None
