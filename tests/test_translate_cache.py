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
from hymt.translate import _segment_cache_hash, _template_options_hash, translate_text


class TranslationCacheTests(unittest.TestCase):
    def test_translate_text_returns_cached_output_without_client_call(self) -> None:
        source = "source text"
        segment_hash = _segment_cache_hash(source)

        with temporary_home():
            HistoryDB().store_segment_cache(
                segment_hash,
                "en",
                TemplateType.DEFAULT.value,
                "cached output",
                "2026-05-23T00:00:00+00:00",
            )
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
        self.assertIn("[1/1] 100.00%", stderr.getvalue())
        client_cls.assert_not_called()

    def test_template_type_is_part_of_segment_cache_identity(self) -> None:
        source = "source text"

        with temporary_home():
            HistoryDB().store_segment_cache(
                _segment_cache_hash(source),
                "en",
                TemplateType.DEFAULT.value,
                "cached output",
                "2026-05-23T00:00:00+00:00",
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
        self.assertEqual(FakeTranslationClient.calls, 1)

    def test_template_options_are_part_of_segment_cache_identity(self) -> None:
        source = "source text"

        with temporary_home():
            HistoryDB().store_segment_cache(
                _segment_cache_hash(source),
                "en",
                TemplateType.STYLE.value,
                "cached output",
                "2026-05-23T00:00:00+00:00",
                options_hash=_template_options_hash({"style": "formal"}),
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
                        style="casual",
                    )
                )

        self.assertEqual(output, "fresh output")
        self.assertEqual(FakeTranslationClient.calls, 1)

    def test_matching_template_options_reuse_segment_cache(self) -> None:
        source = "source text"

        with temporary_home():
            HistoryDB().store_segment_cache(
                _segment_cache_hash(source),
                "en",
                TemplateType.STYLE.value,
                "cached output",
                "2026-05-23T00:00:00+00:00",
                options_hash=_template_options_hash({"style": "formal"}),
            )
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
                        TemplateType.STYLE,
                        style="formal",
                    )
                )

        self.assertEqual(output, "cached output")
        client_cls.assert_not_called()

    def test_translate_text_prompts_and_files_issue_for_timing_divergence(self) -> None:
        source = "source text"

        with temporary_home():
            seed_estimate_history()
            stderr = io.StringIO()
            FakeTranslationClient.calls = 0
            fake_stdin = InteractiveStdin("y\n")
            completed = SimpleNamespace(
                returncode=0,
                stdout="https://github.com/RyderFreeman4Logos/hymt/issues/99\n",
                stderr="",
            )

            with (
                patch("hymt.translate.plan_translation", return_value=FakePlan()),
                patch("hymt.translate._translation_lock", return_value=nullcontext()),
                patch("hymt.translate.TranslationClient", FakeTranslationClient),
                patch(
                    "hymt.translate._monotonic",
                    side_effect=[0.0, 0.0, 5.0, 5.0, 5.0],
                ),
                patch("hymt.timing_issue.sys.stdin", fake_stdin),
                patch("hymt.timing_issue.shutil.which", side_effect=fake_which),
                patch("hymt.timing_issue.platform.platform", return_value="TestOS"),
                patch("hymt.timing_issue.platform.machine", return_value="x86_64"),
                patch("hymt.timing_issue.platform.processor", return_value="test-cpu"),
                patch(
                    "hymt.timing_issue.subprocess.run", return_value=completed
                ) as run,
                redirect_stderr(stderr),
            ):
                output = asyncio.run(
                    translate_text(
                        source,
                        "en",
                        fake_config(),
                        TemplateType.DEFAULT,
                    )
                )

        self.assertEqual(output, "fresh output")
        self.assertIn("Actual time (5s) differs significantly", stderr.getvalue())
        self.assertIn("Filed timing issue", stderr.getvalue())
        run.assert_called_once()
        command = run.call_args.args[0]
        body = command[command.index("--body") + 1]
        self.assertEqual(
            command[:5],
            ["gh", "issue", "create", "--repo", "RyderFreeman4Logos/hymt"],
        )
        self.assertIn("Historical token/s statistics", body)
        self.assertIn("| Input tokens | 11 |", body)
        self.assertIn("| Segments | 1 |", body)
        self.assertIn("- hymt version: 0.1.0", body)
        self.assertIn("- config_version: 1", body)

    def test_translate_text_skips_timing_issue_prompt_when_non_interactive(
        self,
    ) -> None:
        source = "source text"

        with temporary_home():
            seed_estimate_history()
            stderr = io.StringIO()
            FakeTranslationClient.calls = 0

            with (
                patch("hymt.translate.plan_translation", return_value=FakePlan()),
                patch("hymt.translate._translation_lock", return_value=nullcontext()),
                patch("hymt.translate.TranslationClient", FakeTranslationClient),
                patch(
                    "hymt.translate._monotonic",
                    side_effect=[0.0, 0.0, 5.0, 5.0, 5.0],
                ),
                patch("hymt.timing_issue.sys.stdin", io.StringIO("")),
                patch("hymt.timing_issue.subprocess.run") as run,
                redirect_stderr(stderr),
            ):
                output = asyncio.run(
                    translate_text(
                        source,
                        "en",
                        fake_config(),
                        TemplateType.DEFAULT,
                    )
                )

        self.assertEqual(output, "fresh output")
        self.assertNotIn("File an issue with timing data?", stderr.getvalue())
        run.assert_not_called()


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
        return self

    async def __aexit__(self, exc_type: object, exc: object, traceback: object) -> None:
        return None

    async def translate(self, prompt: str) -> str:
        FakeTranslationClient.calls += 1
        return "fresh output"


class InteractiveStdin(io.StringIO):
    def isatty(self) -> bool:
        return True


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


def seed_estimate_history() -> None:
    for index in range(3):
        HistoryDB().insert_task(
            TaskRecord(
                started_at=f"2026-05-23T00:00:0{index}+00:00",
                finished_at=f"2026-05-23T00:00:0{index + 1}+00:00",
                duration_seconds=1.0,
                input_tokens=11,
                output_tokens=10,
                segments=1,
                concurrency=1,
                source_lang=None,
                target_lang="en",
                template_type="default",
                model=None,
                tokens_per_second=10.0,
                input_chars=11,
                output_chars=10,
                output_text=f"historical output {index}",
                input_hash=f"history-{index}",
                config_version=1,
            )
        )


def fake_config() -> SimpleNamespace:
    return SimpleNamespace(
        concurrency=1,
        config_version=1,
        model="test-model",
        timing_divergence_threshold=2.0,
    )


def fake_which(command: str) -> str | None:
    if command == "gh":
        return "/usr/bin/gh"
    return None


if __name__ == "__main__":
    unittest.main()
