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
        self.assertEqual(config.timing_divergence_threshold, 2.0)
        self.assertGreater(plan.available_source_tokens, 0)
        self.assertEqual(plan.segments, ["hello"])

    def test_timing_divergence_threshold_uses_config_value(self) -> None:
        with temporary_home() as home:
            path = os.path.join(home, ".config", "hymt", "config.toml")
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w", encoding="utf-8") as config_file:
                config_file.write(
                    """
[endpoint]
url = "http://127.0.0.1:8401/v1"

[timing]
divergence_threshold = 3.5
"""
                )

            config = HotConfig()

        self.assertEqual(config.timing_divergence_threshold, 3.5)


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
