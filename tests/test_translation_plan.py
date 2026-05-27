from __future__ import annotations

from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from hymt.config import HotConfig
from hymt.language import DocumentLanguagePlan, DocumentSection
from hymt.translate import plan_translation


class TranslationPlanPackingTests(unittest.TestCase):
    def test_packs_adjacent_translatable_paragraphs_into_one_segment(self) -> None:
        text = "First paragraph.\n\nSecond paragraph.\n\nThird paragraph."

        with (
            temporary_config_path() as config_path,
            patch("hymt.translate.create_segmenter", return_value=CountingSegmenter()),
        ):
            plan = plan_translation(text, "zh", HotConfig(config_path))

        self.assertEqual(plan.segments, [text])
        self.assertEqual(plan.segment_section_groups, ((0, 1, 2, 3, 4),))
        self.assertEqual(
            plan.reconstruct(["第一段。\n\n第二段。\n\n第三段。"]),
            "第一段。\n\n第二段。\n\n第三段。",
        )

    def test_kept_sections_break_translation_groups(self) -> None:
        document_plan = DocumentLanguagePlan(
            (
                DocumentSection(
                    "First paragraph.",
                    "paragraph",
                    paragraph_index=1,
                    should_translate=True,
                ),
                DocumentSection("\n\n", "separator"),
                DocumentSection(
                    "中文段落。",
                    "paragraph",
                    paragraph_index=2,
                    is_target_language=True,
                    should_translate=False,
                ),
                DocumentSection("\n\n", "separator"),
                DocumentSection(
                    "Second paragraph.",
                    "paragraph",
                    paragraph_index=3,
                    should_translate=True,
                ),
            ),
            "zh",
        )
        text = "".join(section.text for section in document_plan.sections)

        with (
            temporary_config_path() as config_path,
            patch("hymt.translate.create_segmenter", return_value=CountingSegmenter()),
        ):
            plan = plan_translation(
                text,
                "zh",
                HotConfig(config_path),
                document_plan=document_plan,
            )

        self.assertEqual(plan.segments, ["First paragraph.", "Second paragraph."])
        self.assertEqual(plan.segment_section_groups, ((0,), (4,)))
        self.assertEqual(
            plan.reconstruct(["第一段。", "第二段。"]),
            "第一段。\n\n中文段落。\n\n第二段。",
        )

    def test_reconstruct_combines_split_segments_from_one_section_group(self) -> None:
        text = "First paragraph.\n\nSecond paragraph."

        with (
            temporary_config_path() as config_path,
            patch("hymt.translate.create_segmenter", return_value=SplittingSegmenter()),
        ):
            plan = plan_translation(text, "zh", HotConfig(config_path))

        self.assertEqual(plan.segment_section_groups, ((0, 1, 2), (0, 1, 2)))
        self.assertEqual(
            plan.reconstruct(["第一段。", "\n\n第二段。"]),
            "第一段。\n\n第二段。",
        )


class TranslationPlanBudgetTests(unittest.TestCase):
    def test_source_budget_caps_for_worst_case_one_to_one_expansion(self) -> None:
        # En->zh has a typical ratio of 0.7, but dense technical content
        # (code blocks, tables, paths) translates near 1:1. The budget must
        # cap source tokens so a 1:1 output never exceeds max_output_tokens.
        text = "x" * 200_000

        with (
            temporary_config_path() as config_path,
            patch("hymt.translate.create_segmenter", return_value=CountingSegmenter()),
        ):
            config = HotConfig(config_path)
            plan = plan_translation(text, "zh", config)

        # Worst-case output (assuming 1:1) must stay under max_output_tokens.
        self.assertLess(plan.available_source_tokens, config.max_output_tokens)

    def test_deterministic_segmentation(self) -> None:
        text = (
            "First paragraph with some content.\n\n"
            "Second paragraph with more content.\n\n"
            "Third paragraph here."
        )

        with (
            temporary_config_path() as config_path,
            patch("hymt.translate.create_segmenter", return_value=CountingSegmenter()),
        ):
            config = HotConfig(config_path)
            plan_a = plan_translation(text, "zh", config)
            plan_b = plan_translation(text, "zh", config)

        self.assertEqual(plan_a.segments, plan_b.segments)
        self.assertEqual(plan_a.available_source_tokens, plan_b.available_source_tokens)


class CountingSegmenter:
    def count_tokens(self, text: str) -> int:
        return len(text)

    def segment(self, text: str, max_tokens: int) -> list[str]:
        return [text]


class SplittingSegmenter(CountingSegmenter):
    def segment(self, text: str, max_tokens: int) -> list[str]:
        before_separator, separator, after_separator = text.partition("\n\n")
        return [before_separator, f"{separator}{after_separator}"]


class temporary_config_path:
    def __enter__(self) -> Path:
        self._tmpdir = tempfile.TemporaryDirectory()
        return Path(self._tmpdir.name) / "config.toml"

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        self._tmpdir.cleanup()


if __name__ == "__main__":
    unittest.main()
