from __future__ import annotations

import os
import tempfile
import unittest
from unittest.mock import patch

from hymt.config import HotConfig
from hymt.translate import plan_translation


class DefaultConfigTests(unittest.TestCase):
    def test_default_config_can_plan_non_empty_translation(self) -> None:
        with (
            temporary_home(),
            patch("hymt.translate.create_segmenter", return_value=CountingSegmenter()),
        ):
            config = HotConfig()
            plan = plan_translation("hello", "en", config)

        self.assertEqual(config.context_window, 16384)
        self.assertEqual(config.max_output_tokens, 4096)
        self.assertTrue(config.stream)
        self.assertEqual(config.max_retranslation_retries, 10)
        self.assertEqual(config.primary_lang, "zh")
        self.assertEqual(config.secondary_lang, "en")
        self.assertEqual(config.timing_divergence_threshold, 2.0)
        self.assertEqual(config.completeness_zh_to_en_min_ratio, 0.3)
        self.assertEqual(config.completeness_en_to_zh_min_ratio, 0.4)
        self.assertEqual(config.completeness_min_paragraph_ratio, 0.5)
        self.assertEqual(config.completeness_max_retries, 2)
        self.assertEqual(
            str(config.exec_shared_cache_path), "/usr/local/share/hymt/cache.db"
        )
        self.assertTrue(config.exec_translate_stderr)
        self.assertTrue(config.exec_translate_stdout)
        self.assertEqual(config.exec_skip_patterns, ())
        self.assertEqual(config.exec_skip_commands, ())
        self.assertIn("hymt", config.exec_plugin_blocklist)
        self.assertGreater(plan.available_source_tokens, 0)
        self.assertEqual(plan.segments, ["hello"])

    def test_translation_settings_use_config_values(self) -> None:
        with temporary_home() as home:
            path = os.path.join(home, ".config", "hymt", "config.toml")
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w", encoding="utf-8") as config_file:
                config_file.write(
                    """
[endpoint]
url = "http://127.0.0.1:8401/v1"

[translation]
stream = false
max_retranslation_retries = 7

[language]
primary = "ja"
secondary = "en"

[timing]
divergence_threshold = 3.5

[completeness]
zh_to_en_min_ratio = 0.25
en_to_zh_min_ratio = 0.35
min_paragraph_ratio = 0.6
max_retries = 4

[exec]
translate_stderr = false
translate_stdout = "auto"
skip_patterns = ["*.json"]
skip_commands = ["curl"]

[exec.plugin]
blocklist = ["hymt", "ssh"]
"""
                )

            config = HotConfig()

        self.assertEqual(config.timing_divergence_threshold, 3.5)
        self.assertFalse(config.stream)
        self.assertEqual(config.max_retranslation_retries, 7)
        self.assertEqual(config.primary_lang, "ja")
        self.assertEqual(config.secondary_lang, "en")
        self.assertEqual(config.completeness_zh_to_en_min_ratio, 0.25)
        self.assertEqual(config.completeness_en_to_zh_min_ratio, 0.35)
        self.assertEqual(config.completeness_min_paragraph_ratio, 0.6)
        self.assertEqual(config.completeness_max_retries, 4)
        self.assertFalse(config.exec_translate_stderr)
        self.assertEqual(config.exec_translate_stdout, "auto")
        self.assertEqual(config.exec_skip_patterns, ("*.json",))
        self.assertEqual(config.exec_skip_commands, ("curl",))
        self.assertEqual(config.exec_plugin_blocklist, ("hymt", "ssh"))


class CountingSegmenter:
    def count_tokens(self, text: str) -> int:
        return len(text)

    def segment(self, text: str, max_tokens: int) -> list[str]:
        return [text]


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


if __name__ == "__main__":
    unittest.main()
