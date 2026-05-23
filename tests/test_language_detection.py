from __future__ import annotations

from contextlib import redirect_stderr
import io
import unittest
from unittest.mock import patch

from hymt.cli import _confirm_translation_if_needed
from hymt.language import LanguageDetectionResult


class LanguageDetectionPromptTests(unittest.TestCase):
    def test_confirm_prompt_rejects_predominantly_target_language(self) -> None:
        stderr = io.StringIO()

        with (
            patch("hymt.cli.sys.stdin", InteractiveStdin("n\n")),
            patch(
                "hymt.cli.detect_target_language",
                return_value=LanguageDetectionResult(
                    target_ratio=0.75,
                    detected_lang="en",
                    analyzed_chars=100,
                ),
            ),
            redirect_stderr(stderr),
        ):
            should_translate = _confirm_translation_if_needed(
                "already English", "en", assume_yes=False
            )

        self.assertFalse(should_translate)
        self.assertEqual(
            stderr.getvalue(),
            "Input appears to already be in en. Translate anyway? (y/n) ",
        )

    def test_yes_skips_language_detection(self) -> None:
        with patch("hymt.cli.detect_target_language") as detect:
            should_translate = _confirm_translation_if_needed(
                "already English", "en", assume_yes=True
            )

        self.assertTrue(should_translate)
        detect.assert_not_called()

    def test_non_interactive_stdin_skips_language_detection(self) -> None:
        with (
            patch("hymt.cli.sys.stdin", io.StringIO("")),
            patch("hymt.cli.detect_target_language") as detect,
        ):
            should_translate = _confirm_translation_if_needed(
                "already English", "en", assume_yes=False
            )

        self.assertTrue(should_translate)
        detect.assert_not_called()


class InteractiveStdin(io.StringIO):
    def isatty(self) -> bool:
        return True


if __name__ == "__main__":
    unittest.main()
