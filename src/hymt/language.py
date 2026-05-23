from __future__ import annotations

from collections import Counter
from dataclasses import dataclass
from importlib import import_module
import re
from types import ModuleType


__all__ = ["LanguageDetectionResult", "detect_target_language"]


TARGET_LANGUAGE_ALIASES = {
    "zh": {"zh", "zh-cn", "zh-tw"},
    "zh-cn": {"zh", "zh-cn"},
    "zh-tw": {"zh", "zh-tw"},
    "cn": {"zh", "zh-cn", "zh-tw"},
}


@dataclass(frozen=True)
class LanguageDetectionResult:
    target_ratio: float
    detected_lang: str | None
    analyzed_chars: int


def detect_target_language(
    text: str, target_lang: str
) -> LanguageDetectionResult | None:
    detector = _load_langdetect()
    if detector is None:
        return None

    chunks = _detection_chunks(text)
    if not chunks:
        return None

    aliases = _target_aliases(target_lang)
    detected_counts: Counter[str] = Counter()
    target_chars = 0
    analyzed_chars = 0

    detection_error = _langdetect_error(detector)
    for chunk in chunks:
        try:
            detected = str(detector.detect(chunk)).lower()
        except detection_error:
            continue
        chunk_chars = len(chunk)
        detected_counts[detected] += chunk_chars
        analyzed_chars += chunk_chars
        if detected in aliases:
            target_chars += chunk_chars

    if analyzed_chars == 0:
        return None

    detected_lang = detected_counts.most_common(1)[0][0] if detected_counts else None
    return LanguageDetectionResult(
        target_ratio=target_chars / analyzed_chars,
        detected_lang=detected_lang,
        analyzed_chars=analyzed_chars,
    )


def _load_langdetect() -> ModuleType | None:
    try:
        return import_module("langdetect")
    except ImportError:
        return None


def _langdetect_error(detector: ModuleType) -> type[BaseException]:
    exception_module = getattr(detector, "lang_detect_exception", None)
    exception_type = getattr(exception_module, "LangDetectException", None)
    if isinstance(exception_type, type):
        return exception_type
    try:
        imported_module = import_module("langdetect.lang_detect_exception")
    except ImportError:
        return ValueError
    imported_type = getattr(imported_module, "LangDetectException", None)
    return imported_type if isinstance(imported_type, type) else ValueError


def _target_aliases(target_lang: str) -> set[str]:
    normalized = target_lang.strip().lower()
    return TARGET_LANGUAGE_ALIASES.get(normalized, {normalized})


def _detection_chunks(text: str) -> list[str]:
    chunks = [chunk.strip() for chunk in re.split(r"\n\s*\n", text) if chunk.strip()]
    if not chunks and text.strip():
        chunks = [text.strip()]
    return chunks
