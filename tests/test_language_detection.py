from __future__ import annotations

from contextlib import redirect_stderr
import io
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from hymt.cli import _select_document_translation_plan
from hymt.language import analyze_document_language, resolve_target_language


class LanguageDetectionPromptTests(unittest.TestCase):
    def test_rejects_fully_target_language_translation(self) -> None:
        stderr = io.StringIO()

        with (
            patch("hymt.cli.sys.stdin", InteractiveStdin("n\n")),
            patch("hymt.language._load_langdetect", return_value=FakeDetector()),
            redirect_stderr(stderr),
        ):
            plan = _select_document_translation_plan(
                "Already English.", "en", assume_yes=False
            )

        self.assertIsNone(plan)
        self.assertEqual(
            stderr.getvalue(),
            "Input appears to already be in en. Translate anyway? (y/n) ",
        )

    def test_mixed_language_prompt_selects_partial_translation(self) -> None:
        stderr = io.StringIO()

        with (
            patch("hymt.cli.sys.stdin", InteractiveStdin("y\n")),
            patch("hymt.language._load_langdetect", return_value=FakeDetector()),
            redirect_stderr(stderr),
        ):
            plan = _select_document_translation_plan(
                "English paragraph.\n\n中文段落。", "zh", assume_yes=False
            )

        self.assertIsNotNone(plan)
        if plan is None:
            self.fail("Expected partial translation plan")
        self.assertEqual(plan.paragraph_count, 2)
        self.assertEqual(plan.target_paragraph_count, 1)
        self.assertEqual(plan.translate_paragraph_count, 1)
        self.assertIn("Partial translation plan:", stderr.getvalue())
        self.assertIn("[1] translate (en): English paragraph.", stderr.getvalue())
        self.assertIn("[2] keep (zh): 中文段落。", stderr.getvalue())
        self.assertIn(
            "1 of 2 paragraphs are already in zh. "
            "Translate only the remaining 1 paragraphs? (y/n/all) ",
            stderr.getvalue(),
        )

    def test_mixed_language_prompt_all_translates_every_paragraph(self) -> None:
        with (
            patch("hymt.cli.sys.stdin", InteractiveStdin("all\n")),
            patch("hymt.language._load_langdetect", return_value=FakeDetector()),
            redirect_stderr(io.StringIO()),
        ):
            plan = _select_document_translation_plan(
                "English paragraph.\n\n中文段落。", "zh", assume_yes=False
            )

        self.assertIsNotNone(plan)
        if plan is None:
            self.fail("Expected translation plan")
        self.assertEqual(plan.translate_paragraph_count, 2)

    def test_yes_auto_selects_partial_translation(self) -> None:
        with (
            patch("hymt.language._load_langdetect", return_value=FakeDetector()),
            redirect_stderr(io.StringIO()),
        ):
            plan = _select_document_translation_plan(
                "English paragraph.\n\n中文段落。", "zh", assume_yes=True
            )

        self.assertIsNotNone(plan)
        if plan is None:
            self.fail("Expected partial translation plan")
        self.assertEqual(plan.translate_paragraph_count, 1)

    def test_non_interactive_stdin_auto_selects_partial_translation(self) -> None:
        with (
            patch("hymt.cli.sys.stdin", io.StringIO("")),
            patch("hymt.language._load_langdetect", return_value=FakeDetector()),
            redirect_stderr(io.StringIO()),
        ):
            plan = _select_document_translation_plan(
                "English paragraph.\n\n中文段落。", "zh", assume_yes=False
            )

        self.assertIsNotNone(plan)
        if plan is None:
            self.fail("Expected partial translation plan")
        self.assertEqual(plan.translate_paragraph_count, 1)


class DocumentLanguageAnalysisTests(unittest.TestCase):
    def test_code_blocks_are_not_counted_or_translated(self) -> None:
        text = "English paragraph.\n\n```python\nprint('hello')\n```\n\n中文段落。"

        with patch("hymt.language._load_langdetect", return_value=FakeDetector()):
            plan = analyze_document_language(text, "zh")

        self.assertEqual(plan.paragraph_count, 2)
        self.assertEqual(plan.target_paragraph_count, 1)
        self.assertEqual(plan.translate_paragraph_count, 1)
        code_sections = [section for section in plan.sections if section.kind == "code"]
        self.assertEqual(len(code_sections), 1)
        self.assertFalse(code_sections[0].should_translate)


class TargetLanguageRoutingTests(unittest.TestCase):
    def test_default_route_reverses_mostly_primary_language_text(self) -> None:
        config = SimpleNamespace(primary_lang="zh", secondary_lang="en")

        with patch("hymt.language._load_langdetect", return_value=FakeDetector()):
            target_lang = resolve_target_language(
                "中文段落。", "zh", config, explicit_target=False
            )

        self.assertEqual(target_lang, "en")

    def test_default_route_detects_chinese_without_optional_detector(self) -> None:
        config = SimpleNamespace(primary_lang="zh", secondary_lang="en")

        with patch("hymt.language._load_langdetect", return_value=None):
            target_lang = resolve_target_language(
                "中文段落。", "zh", config, explicit_target=False
            )

        self.assertEqual(target_lang, "en")

    def test_default_route_uses_primary_for_secondary_language_text(self) -> None:
        config = SimpleNamespace(primary_lang="zh", secondary_lang="en")

        with patch("hymt.language._load_langdetect", return_value=FakeDetector()):
            target_lang = resolve_target_language(
                "English paragraph.", "zh", config, explicit_target=False
            )

        self.assertEqual(target_lang, "zh")

    def test_explicit_target_disables_reverse_routing(self) -> None:
        config = SimpleNamespace(primary_lang="zh", secondary_lang="en")

        with patch("hymt.language._load_langdetect", return_value=FakeDetector()):
            target_lang = resolve_target_language(
                "中文段落。", "zh", config, explicit_target=True
            )

        self.assertEqual(target_lang, "zh")


class FakeDetector:
    def detect(self, text: str) -> str:
        if any("\u4e00" <= char <= "\u9fff" for char in text):
            return "zh"
        return "en"


class InteractiveStdin(io.StringIO):
    def isatty(self) -> bool:
        return True


if __name__ == "__main__":
    unittest.main()
