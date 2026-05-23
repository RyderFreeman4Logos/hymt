from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
import re
from typing import Protocol

try:
    from tokenizers import Tokenizer as _TokenizerImpl
except ImportError:
    _TokenizerImpl = None


TOKENIZER_REPO = "tencent/Hy-MT2-7B"
TOKENIZER_FILENAME = "tokenizer.json"
TOKENIZER_CACHE_DIR = Path.home() / ".cache" / "hymt" / "tokenizer"
TOKENIZER_PATH = TOKENIZER_CACHE_DIR / TOKENIZER_FILENAME

_PARAGRAPH_SPLIT_RE = re.compile(r"(\n\s*\n)")
_WORD_SPLIT_RE = re.compile(r"(\s+)")
_CJK_SENTENCE_ENDERS = frozenset("。！？")
_EN_SENTENCE_ENDERS = frozenset(".!?")
_CLAUSE_ENDERS = frozenset("，,、；;：:")
_TRAILING_CLOSERS = frozenset("\"'”’」』）)]】》〉")
_COMMON_ABBREVIATIONS = frozenset(
    {
        "dr",
        "mr",
        "mrs",
        "ms",
        "prof",
        "sr",
        "jr",
        "st",
        "vs",
        "etc",
        "e.g",
        "i.e",
        "fig",
        "no",
    }
)


class _EncodedTokens(Protocol):
    ids: list[int]


class _TokenizerLike(Protocol):
    def encode(self, text: str) -> _EncodedTokens: ...


class Segmenter:
    def __init__(self, tokenizer_path: str | None = None) -> None:
        self._tokenizer: _TokenizerLike | None = None
        if tokenizer_path is not None and _TokenizerImpl is not None:
            self._tokenizer = _TokenizerImpl.from_file(str(tokenizer_path))

    def count_tokens(self, text: str) -> int:
        if self._tokenizer is None:
            return _estimate_token_count(text)
        return len(self._tokenizer.encode(text).ids)

    def segment(self, text: str, max_tokens: int) -> list[str]:
        if max_tokens <= 0:
            raise ValueError("max_tokens must be positive")
        if not text:
            return []
        if self.count_tokens(text) <= max_tokens:
            return [text]

        units: list[str] = []
        for paragraph in _split_paragraphs(text):
            if self.count_tokens(paragraph) <= max_tokens:
                units.append(paragraph)
                continue
            for sentence in _split_sentences(paragraph):
                if self.count_tokens(sentence) <= max_tokens:
                    units.append(sentence)
                    continue
                for clause in _split_clauses(sentence):
                    if self.count_tokens(clause) <= max_tokens:
                        units.append(clause)
                        continue
                    units.extend(self._split_word_or_character(clause, max_tokens))

        return self._pack_units(units, max_tokens)

    def _split_word_or_character(self, text: str, max_tokens: int) -> list[str]:
        word_units = [part for part in _WORD_SPLIT_RE.split(text) if part]
        if len(word_units) > 1:
            chunks: list[str] = []
            for unit in word_units:
                if self.count_tokens(unit) <= max_tokens:
                    chunks.append(unit)
                else:
                    chunks.extend(self._split_characters(unit, max_tokens))
            return self._pack_units(chunks, max_tokens)
        return self._split_characters(text, max_tokens)

    def _split_characters(self, text: str, max_tokens: int) -> list[str]:
        chunks: list[str] = []
        current = ""
        for char in text:
            if self.count_tokens(char) > max_tokens:
                raise ValueError("max_tokens is too small for the tokenizer output")
            candidate = current + char
            if current and self.count_tokens(candidate) > max_tokens:
                chunks.append(current)
                current = char
            else:
                current = candidate
        if current:
            chunks.append(current)
        return chunks

    def _pack_units(self, units: list[str], max_tokens: int) -> list[str]:
        segments: list[str] = []
        current = ""
        for unit in units:
            if not unit:
                continue
            if self.count_tokens(unit) > max_tokens:
                raise ValueError("Internal segmentation error: unit exceeds max_tokens")
            candidate = current + unit
            if current and self.count_tokens(candidate) > max_tokens:
                segments.append(current)
                current = unit
            else:
                current = candidate
        if current:
            segments.append(current)
        return segments


def create_segmenter(force_download: bool = False) -> Segmenter:
    if not has_tokenizer_support():
        return Segmenter()
    return Segmenter(ensure_tokenizer(force_download=force_download))


def ensure_tokenizer(force_download: bool = False) -> str:
    if not has_tokenizer_support():
        raise RuntimeError("tokenizers is not installed; using approximate token counting")
    try:
        from huggingface_hub import hf_hub_download
    except ImportError as exc:
        raise RuntimeError("huggingface-hub is required to download the tokenizer") from exc

    if TOKENIZER_PATH.exists() and not force_download:
        return str(TOKENIZER_PATH)
    TOKENIZER_CACHE_DIR.mkdir(parents=True, exist_ok=True)
    path = hf_hub_download(
        repo_id=TOKENIZER_REPO,
        filename=TOKENIZER_FILENAME,
        local_dir=str(TOKENIZER_CACHE_DIR),
        force_download=force_download,
    )
    return str(Path(path))


def has_tokenizer_support() -> bool:
    return _TokenizerImpl is not None


def _estimate_token_count(text: str) -> int:
    if not text:
        return 0
    return max(1, (len(text.encode("utf-8")) + 3) // 4)


def _split_paragraphs(text: str) -> list[str]:
    parts = _PARAGRAPH_SPLIT_RE.split(text)
    paragraphs: list[str] = []
    for index in range(0, len(parts), 2):
        paragraph = parts[index]
        separator = parts[index + 1] if index + 1 < len(parts) else ""
        unit = paragraph + separator
        if unit:
            paragraphs.append(unit)
    return paragraphs


def _split_sentences(text: str) -> list[str]:
    return _split_on_boundaries(text, _sentence_boundary_end)


def _split_clauses(text: str) -> list[str]:
    return _split_on_boundaries(text, _clause_boundary_end)


def _split_on_boundaries(
    text: str,
    boundary_end_at: Callable[[str, int], int | None],
) -> list[str]:
    parts: list[str] = []
    start = 0
    index = 0
    while index < len(text):
        boundary_end = boundary_end_at(text, index)
        if boundary_end is None:
            index += 1
            continue
        split_at = _consume_trailing_closers(text, boundary_end)
        parts.append(text[start:split_at])
        start = split_at
        index = split_at
    if start < len(text):
        parts.append(text[start:])
    return [part for part in parts if part]


def _sentence_boundary_end(text: str, index: int) -> int | None:
    char = text[index]
    if char == "…":
        return _consume_consecutive(text, index, "…")
    if char in _CJK_SENTENCE_ENDERS:
        return _consume_sentence_enders(text, index)
    if char not in _EN_SENTENCE_ENDERS:
        return None
    if char == "." and _is_decimal_point(text, index):
        return None

    boundary_end = _consume_sentence_enders(text, index)
    lookahead = _consume_trailing_closers(text, boundary_end)
    if lookahead < len(text) and text[boundary_end - 1] == "." and _looks_like_abbreviation(
        text, boundary_end - 1
    ):
        return None
    if _starts_sentence_after_boundary(text, lookahead):
        return boundary_end
    return None


def _clause_boundary_end(text: str, index: int) -> int | None:
    if text[index] in _CLAUSE_ENDERS:
        return index + 1
    return None


def _consume_sentence_enders(text: str, index: int) -> int:
    end = index
    while end < len(text):
        char = text[end]
        if char == "…":
            end = _consume_consecutive(text, end, "…")
            continue
        if char in _CJK_SENTENCE_ENDERS or char in _EN_SENTENCE_ENDERS:
            end += 1
            continue
        break
    return end


def _consume_consecutive(text: str, index: int, char: str) -> int:
    end = index
    while end < len(text) and text[end] == char:
        end += 1
    return end


def _consume_trailing_closers(text: str, index: int) -> int:
    end = index
    while end < len(text) and text[end] in _TRAILING_CLOSERS:
        end += 1
    return end


def _starts_sentence_after_boundary(text: str, index: int) -> bool:
    if index >= len(text):
        return True

    lookahead = index
    while lookahead < len(text) and text[lookahead].isspace():
        lookahead += 1
    if lookahead == index:
        return False
    if lookahead >= len(text):
        return True

    next_char = text[lookahead]
    return next_char.isupper() or _is_cjk_character(next_char)


def _is_decimal_point(text: str, index: int) -> bool:
    return 0 < index < len(text) - 1 and text[index - 1].isdigit() and text[index + 1].isdigit()


def _looks_like_abbreviation(text: str, index: int) -> bool:
    start = index
    while start > 0 and (text[start - 1].isalpha() or text[start - 1] == "."):
        start -= 1

    candidate = text[start:index]
    if not candidate or not any(char.isalpha() for char in candidate):
        return False

    normalized = candidate.casefold()
    if normalized in _COMMON_ABBREVIATIONS:
        return True

    parts = [part for part in candidate.split(".") if part]
    return len(parts) > 1 and all(len(part) == 1 and part.isalpha() for part in parts)


def _is_cjk_character(char: str) -> bool:
    codepoint = ord(char)
    return (
        0x3400 <= codepoint <= 0x4DBF
        or 0x4E00 <= codepoint <= 0x9FFF
        or 0x3040 <= codepoint <= 0x30FF
        or 0xAC00 <= codepoint <= 0xD7AF
    )
