from __future__ import annotations

import asyncio
from contextlib import nullcontext, redirect_stderr
import io
import os
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from hymt.history import HistoryDB, TaskRecord
from hymt.templates import TemplateType
from hymt.translate import _translation_cache_hash, translate_text


class TranslationCacheTests(unittest.TestCase):
    def test_translate_text_returns_cached_output_without_client_call(self) -> None:
        source = "source text"
        input_hash = _translation_cache_hash(source, "en", TemplateType.DEFAULT, {})

        with temporary_home():
            HistoryDB().insert_task(task_record("cached output", input_hash=input_hash))
            stderr = io.StringIO()

            with (
                patch("hymt.translate.plan_translation", return_value=FakePlan()),
                patch("hymt.translate.TranslationClient") as client_cls,
                redirect_stderr(stderr),
            ):
                output = asyncio.run(
                    translate_text(
                        source,
                        "en",
                        SimpleNamespace(concurrency=1, config_version=1, model=""),
                        TemplateType.DEFAULT,
                    )
                )

        self.assertEqual(output, "cached output")
        self.assertIn("Source tokens: 11; segments: 1", stderr.getvalue())
        self.assertIn("Cache hit", stderr.getvalue())
        client_cls.assert_not_called()

    def test_template_options_are_part_of_cache_identity(self) -> None:
        source = "source text"
        cached_hash = _translation_cache_hash(
            source,
            "en",
            TemplateType.STYLE,
            {"style": "casual"},
        )

        with temporary_home():
            HistoryDB().insert_task(
                task_record(
                    "cached output",
                    input_hash=cached_hash,
                    template_type=TemplateType.STYLE.value,
                )
            )
            stderr = io.StringIO()
            FakeTranslationClient.calls = 0

            with (
                patch("hymt.translate.plan_translation", return_value=FakePlan()),
                patch("hymt.translate._translation_lock", return_value=nullcontext()),
                patch("hymt.translate.TranslationClient", FakeTranslationClient),
                redirect_stderr(stderr),
            ):
                output = asyncio.run(
                    translate_text(
                        source,
                        "en",
                        SimpleNamespace(concurrency=1, config_version=1, model=""),
                        TemplateType.STYLE,
                        style="formal",
                    )
                )

        self.assertEqual(output, "fresh output")
        self.assertNotIn("Cache hit", stderr.getvalue())
        self.assertEqual(FakeTranslationClient.calls, 1)


class FakePlan:
    source_tokens = 11
    segments = ["source text"]

    @property
    def segment_count(self) -> int:
        return len(self.segments)

    def count_tokens(self, text: str) -> int:
        return len(text)


class FakeTranslationClient:
    calls = 0

    def __init__(self, config: object) -> None:
        self._config = config

    async def __aenter__(self) -> FakeTranslationClient:
        FakeTranslationClient.calls += 1
        return self

    async def __aexit__(self, exc_type: object, exc: object, traceback: object) -> None:
        return None

    async def translate_batch(
        self,
        prompts: list[str],
        on_progress: object,
    ) -> list[str]:
        return ["fresh output"]


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


def task_record(
    output_text: str,
    *,
    input_hash: str,
    template_type: str = "default",
) -> TaskRecord:
    return TaskRecord(
        started_at="2026-05-23T00:00:00+00:00",
        finished_at="2026-05-23T00:00:01+00:00",
        duration_seconds=1.0,
        input_tokens=1,
        output_tokens=1,
        segments=1,
        concurrency=1,
        source_lang=None,
        target_lang="en",
        template_type=template_type,
        model=None,
        tokens_per_second=1.0,
        input_chars=11,
        output_chars=len(output_text),
        output_text=output_text,
        input_hash=input_hash,
    )


if __name__ == "__main__":
    unittest.main()
