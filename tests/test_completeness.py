from __future__ import annotations

import asyncio
from contextlib import nullcontext, redirect_stderr
import io
import os
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from hymt.completeness import (
    CompletenessResult,
    CompletenessThresholds,
    validate_completeness,
)
from hymt.templates import TemplateType
from hymt.translate import translate_text


class CompletenessValidatorTests(unittest.TestCase):
    def test_token_ratio_uses_directional_thresholds(self) -> None:
        self.assertFalse(validate_completeness("a" * 100, "b" * 29, "zh").is_complete)
        self.assertTrue(validate_completeness("a" * 100, "b" * 30, "zh").is_complete)
        self.assertFalse(validate_completeness("a" * 100, "b" * 29, "en").is_complete)
        self.assertTrue(validate_completeness("a" * 100, "b" * 30, "en").is_complete)

    def test_token_ratio_thresholds_are_configurable(self) -> None:
        result = validate_completeness(
            "a" * 100,
            "b" * 35,
            "zh",
            CompletenessThresholds(en_to_zh_min_ratio=0.3),
        )

        self.assertTrue(result.is_complete)

    def test_paragraph_count_check_flags_large_drop(self) -> None:
        source = "one\n\ntwo\n\nthree\n\nfour"
        failing = validate_completeness(source, "one", "ja")
        passing = validate_completeness(source, "one\n\ntwo", "ja")

        self.assertIn("paragraph_count", failing.checks_failed)
        self.assertTrue(passing.is_complete)

    def test_heading_preservation_check_flags_missing_headings(self) -> None:
        source = "# One\n\nbody\n\n## Two\n\nbody"
        failing = validate_completeness(source, "# One\n\nbody", "ja")
        passing = validate_completeness(source, "# One\n\nbody\n\n## Two\n\nbody", "ja")

        self.assertIn("heading_preservation", failing.checks_failed)
        self.assertTrue(passing.is_complete)

    def test_result_dataclass_reports_stats(self) -> None:
        result = validate_completeness("# One\n\nbody", "short", "ja")

        self.assertIsInstance(result, CompletenessResult)
        self.assertFalse(result.is_complete)
        self.assertEqual(result.input_stats.char_count, 11)
        self.assertEqual(result.input_stats.paragraph_count, 2)
        self.assertEqual(result.input_stats.heading_count, 1)
        self.assertEqual(result.output_stats.char_count, 5)


class TranslationCompletenessRetryTests(unittest.TestCase):
    def test_incomplete_segment_is_retried(self) -> None:
        source = (
            "# First section\n\n"
            "This segment has a body that should survive translation.\n\n"
            "## Second section\n\n"
            "The second section must not be silently dropped."
        )
        complete_output = (
            "# First section translated\n\n"
            "This translated body is long enough to satisfy the ratio check.\n\n"
            "## Second section translated\n\n"
            "The second translated body is present too."
        )
        RetryingClient.responses = [
            "Only the first body was translated.",
            complete_output,
        ]
        RetryingClient.prompts = []

        with temporary_home():
            stderr = io.StringIO()
            with (
                patch("hymt.translate.plan_translation", return_value=FakePlan(source)),
                patch("hymt.translate._translation_lock", return_value=nullcontext()),
                patch("hymt.translate.TranslationClient", RetryingClient),
                redirect_stderr(stderr),
            ):
                output = asyncio.run(
                    translate_text(
                        source,
                        "zh",
                        fake_config(),
                        TemplateType.DEFAULT,
                        stream=False,
                    )
                )

        self.assertEqual(output, complete_output)
        self.assertEqual(len(RetryingClient.prompts), 2)
        self.assertIn("Translate the COMPLETE input", RetryingClient.prompts[1])
        self.assertIn("failed completeness validation", stderr.getvalue())


class FakePlan:
    document_plan = None
    segment_section_indexes: tuple[int, ...] = ()
    segment_section_groups: tuple[tuple[int, ...], ...] = ()

    def __init__(self, segment: str) -> None:
        self.source_tokens = len(segment)
        self.segments = [segment]

    @property
    def segment_count(self) -> int:
        return len(self.segments)

    def count_tokens(self, text: str) -> int:
        return len(text)

    def reconstruct(self, translations: list[str]) -> str:
        return "".join(translations)


class RetryingClient:
    responses: list[str] = []
    prompts: list[str] = []

    def __init__(self, config: object) -> None:
        self._config = config

    async def __aenter__(self) -> RetryingClient:
        return self

    async def __aexit__(self, exc_type: object, exc: object, traceback: object) -> None:
        return None

    async def translate(self, prompt: str) -> str:
        self.prompts.append(prompt)
        return self.responses[len(self.prompts) - 1]


class temporary_home:
    def __enter__(self) -> str:
        self._tmpdir = tempfile.TemporaryDirectory()
        self._old_home = os.environ.get("HOME")
        os.environ["HOME"] = self._tmpdir.name
        return self._tmpdir.name

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        if self._old_home is None:
            os.environ.pop("HOME", None)
        else:
            os.environ["HOME"] = self._old_home
        self._tmpdir.cleanup()


def fake_config() -> SimpleNamespace:
    return SimpleNamespace(
        context_window=4096,
        max_output_tokens=128,
        concurrency=1,
        config_version=1,
        model="test-model",
        timing_divergence_threshold=2.0,
        completeness_zh_to_en_min_ratio=0.3,
        completeness_en_to_zh_min_ratio=0.4,
        completeness_min_paragraph_ratio=0.5,
        completeness_max_retries=2,
    )


if __name__ == "__main__":
    unittest.main()
