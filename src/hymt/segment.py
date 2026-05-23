from __future__ import annotations

from pathlib import Path
import re

from huggingface_hub import hf_hub_download
from tokenizers import Tokenizer


TOKENIZER_REPO = "tencent/Hy-MT2-7B"
TOKENIZER_FILENAME = "tokenizer.json"
TOKENIZER_CACHE_DIR = Path.home() / ".cache" / "hymt" / "tokenizer"
TOKENIZER_PATH = TOKENIZER_CACHE_DIR / TOKENIZER_FILENAME

_CJK_SENTENCE_RE = re.compile(
    r"(?<=[。！？…])"
    r"[\"'」』）)\]]*"
    r"(?=\s|\Z|[^\"'」』）)\]])"
)

_EN_SENTENCE_RE = re.compile(
    r"(?<=[.!?])"
    r"[\"')]*"
    r"(?=\s+[A-Z一-鿿぀-ヿ]|\s*\Z)"
)

_CLAUSE_RE = re.compile(
    r"(?<=[，,、；;：:])"
    r"[\"'」』）)]*"
    r"(?=\s|\Z|[^\"'」』）)])"
)


class Segmenter:
    def __init__(self, tokenizer_path: str) -> None:
        self._tokenizer = Tokenizer.from_file(str(tokenizer_path))

    def count_tokens(self, text: str) -> int:
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
        word_units = [part for part in re.split(r"(\s+)", text) if part]
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


def ensure_tokenizer(force_download: bool = False) -> str:
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


def _split_paragraphs(text: str) -> list[str]:
    parts = re.split(r"(\n\s*\n)", text)
    paragraphs: list[str] = []
    for index in range(0, len(parts), 2):
        paragraph = parts[index]
        separator = parts[index + 1] if index + 1 < len(parts) else ""
        unit = paragraph + separator
        if unit:
            paragraphs.append(unit)
    return paragraphs


def _split_sentences(text: str) -> list[str]:
    parts = _CJK_SENTENCE_RE.split(text)
    result: list[str] = []
    for part in parts:
        if not part:
            continue
        en_parts = _EN_SENTENCE_RE.split(part)
        result.extend(p for p in en_parts if p)
    return result


def _split_clauses(text: str) -> list[str]:
    parts = _CLAUSE_RE.split(text)
    return [p for p in parts if p]
