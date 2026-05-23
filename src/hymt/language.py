from __future__ import annotations

from collections import Counter
from dataclasses import dataclass, replace
from importlib import import_module
import re
from types import ModuleType
from typing import Literal, TypeAlias


__all__ = [
    "DocumentLanguagePlan",
    "DocumentSection",
    "LanguageDetectionResult",
    "analyze_document_language",
    "build_document_translation_plan",
    "detect_target_language",
]


DocumentSectionKind: TypeAlias = Literal["paragraph", "separator", "code"]
TARGET_PARAGRAPH_RATIO = 0.60


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


@dataclass(frozen=True)
class DocumentSection:
    text: str
    kind: DocumentSectionKind
    paragraph_index: int | None = None
    detected_lang: str | None = None
    target_ratio: float | None = None
    analyzed_chars: int = 0
    is_target_language: bool = False
    should_translate: bool = False


@dataclass(frozen=True)
class DocumentLanguagePlan:
    sections: tuple[DocumentSection, ...]
    target_lang: str

    @property
    def paragraph_count(self) -> int:
        return sum(1 for section in self.sections if section.kind == "paragraph")

    @property
    def target_paragraph_count(self) -> int:
        return sum(1 for section in self.sections if section.is_target_language)

    @property
    def translate_paragraph_count(self) -> int:
        return sum(1 for section in self.sections if section.should_translate)

    @property
    def has_mixed_language(self) -> bool:
        return 0 < self.target_paragraph_count < self.paragraph_count

    def translate_all_paragraphs(self) -> DocumentLanguagePlan:
        return DocumentLanguagePlan(
            tuple(
                replace(section, should_translate=section.kind == "paragraph")
                for section in self.sections
            ),
            self.target_lang,
        )


def detect_target_language(
    text: str, target_lang: str
) -> LanguageDetectionResult | None:
    detector = _load_langdetect()
    if detector is None:
        return None

    return _detect_target_language_with_detector(
        text,
        _target_aliases(target_lang),
        detector,
        _langdetect_error(detector),
    )


def analyze_document_language(text: str, target_lang: str) -> DocumentLanguagePlan:
    detector = _load_langdetect()
    detection_error = _langdetect_error(detector) if detector is not None else None
    aliases = _target_aliases(target_lang)
    sections: list[DocumentSection] = []

    for raw_section in _split_document_sections(text):
        if raw_section.kind != "paragraph":
            sections.append(raw_section)
            continue
        detection = (
            _detect_target_language_with_detector(
                raw_section.text, aliases, detector, detection_error
            )
            if detector is not None and detection_error is not None
            else None
        )
        is_target = (
            detection is not None and detection.target_ratio > TARGET_PARAGRAPH_RATIO
        )
        sections.append(
            replace(
                raw_section,
                detected_lang=detection.detected_lang if detection else None,
                target_ratio=detection.target_ratio if detection else None,
                analyzed_chars=detection.analyzed_chars if detection else 0,
                is_target_language=is_target,
                should_translate=not is_target,
            )
        )

    return DocumentLanguagePlan(tuple(sections), target_lang)


def build_document_translation_plan(
    text: str, target_lang: str
) -> DocumentLanguagePlan:
    return DocumentLanguagePlan(
        tuple(
            replace(section, should_translate=section.kind == "paragraph")
            for section in _split_document_sections(text)
        ),
        target_lang,
    )


def _detect_target_language_with_detector(
    text: str,
    aliases: set[str],
    detector: ModuleType,
    detection_error: type[BaseException],
) -> LanguageDetectionResult | None:
    chunks = _detection_chunks(text)
    if not chunks:
        return None

    detected_counts: Counter[str] = Counter()
    target_chars = 0
    analyzed_chars = 0

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


def _split_document_sections(text: str) -> list[DocumentSection]:
    sections: list[DocumentSection] = []
    paragraph_lines: list[str] = []
    code_lines: list[str] = []
    fence_char = ""
    fence_len = 0
    paragraph_index = 0

    def flush_paragraph() -> None:
        nonlocal paragraph_index, paragraph_lines
        if not paragraph_lines:
            return
        paragraph_index += 1
        sections.append(
            DocumentSection(
                text="".join(paragraph_lines),
                kind="paragraph",
                paragraph_index=paragraph_index,
            )
        )
        paragraph_lines = []

    for line in text.splitlines(keepends=True):
        if code_lines:
            code_lines.append(line)
            if _is_closing_fence(line, fence_char, fence_len):
                sections.append(DocumentSection("".join(code_lines), "code"))
                code_lines = []
                fence_char = ""
                fence_len = 0
            continue

        opening_fence = _opening_fence(line)
        if opening_fence is not None:
            flush_paragraph()
            fence_char, fence_len = opening_fence
            code_lines = [line]
            continue

        if not line.strip():
            flush_paragraph()
            sections.append(DocumentSection(line, "separator"))
            continue

        paragraph_lines.append(line)

    if code_lines:
        sections.append(DocumentSection("".join(code_lines), "code"))
    flush_paragraph()
    return sections


_OPENING_FENCE_RE = re.compile(r"^[ \t]*(?P<fence>`{3,}|~{3,})")


def _opening_fence(line: str) -> tuple[str, int] | None:
    match = _OPENING_FENCE_RE.match(line)
    if match is None:
        return None
    fence = match.group("fence")
    return fence[0], len(fence)


def _is_closing_fence(line: str, fence_char: str, fence_len: int) -> bool:
    stripped = line.strip()
    if not stripped:
        return False
    index = 0
    while index < len(stripped) and stripped[index] == fence_char:
        index += 1
    return index >= fence_len and not stripped[index:].strip()
