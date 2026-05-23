from __future__ import annotations

import unittest
from types import SimpleNamespace

from hymt.segment import Segmenter, _split_clauses, _split_sentences


class SentenceSplitTests(unittest.TestCase):
    def test_split_sentences_handles_cjk_boundaries(self) -> None:
        text = "这是第一句话。这是第二句？第三句！"
        self.assertEqual(
            _split_sentences(text),
            ["这是第一句话。", "这是第二句？", "第三句！"],
        )

    def test_split_sentences_preserves_decimal_points(self) -> None:
        text = "The value is 3.14. This is next."
        self.assertEqual(
            _split_sentences(text),
            ["The value is 3.14.", " This is next."],
        )

    def test_split_sentences_keeps_closing_quotes_with_sentence(self) -> None:
        text = '他说："你好吗？"然后走了。'
        self.assertEqual(
            _split_sentences(text),
            ['他说："你好吗？"', "然后走了。"],
        )

    def test_split_sentences_treats_consecutive_ellipsis_as_one_boundary(self) -> None:
        text = "他走了……然后她来了。"
        self.assertEqual(
            _split_sentences(text),
            ["他走了……", "然后她来了。"],
        )

    def test_split_sentences_avoids_common_abbreviation_breaks(self) -> None:
        text = "Dr. Smith arrived. Another sentence."
        self.assertEqual(
            _split_sentences(text),
            ["Dr. Smith arrived.", " Another sentence."],
        )


class ClauseSplitTests(unittest.TestCase):
    def test_split_clauses_preserves_delimiters(self) -> None:
        text = "第一子句，第二子句；第三子句"
        self.assertEqual(
            _split_clauses(text),
            ["第一子句，", "第二子句；", "第三子句"],
        )


class SegmenterTests(unittest.TestCase):
    def test_segment_uses_sentence_then_clause_fallback(self) -> None:
        segmenter = make_segmenter()
        text = "第一句。第二句，第三句；第四句"
        self.assertEqual(
            segmenter.segment(text, max_tokens=4),
            ["第一句。", "第二句，", "第三句；", "第四句"],
        )


class FakeTokenizer:
    def encode(self, text: str) -> SimpleNamespace:
        return SimpleNamespace(ids=list(range(len(text))))


def make_segmenter() -> Segmenter:
    segmenter = Segmenter.__new__(Segmenter)
    segmenter._tokenizer = FakeTokenizer()
    return segmenter


if __name__ == "__main__":
    unittest.main()
