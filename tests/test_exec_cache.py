from __future__ import annotations

import asyncio
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import AsyncMock, patch

from hymt.config import HotConfig
from hymt.exec_cache import ExecCache, translate_cached_text
from hymt.templates import TemplateType


class ExecCacheTests(unittest.TestCase):
    def test_user_cache_wins_over_shared_cache(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            shared = Path(tmpdir) / "shared.db"
            user = Path(tmpdir) / "user.db"
            cache = ExecCache(shared, user)

            cache.store_shared("man", "git", "manual", "zh", "shared translation")
            cache.store_user("man", "git", "manual", "zh", "user translation")

            cached = cache.find("man", "git", "manual", "zh")

        self.assertEqual(cached, "user translation")

    def test_shared_cache_is_used_after_user_miss(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            shared = Path(tmpdir) / "shared.db"
            user = Path(tmpdir) / "user.db"
            writer = ExecCache(shared, user)
            writer.store_shared("info", "coreutils", "manual", "zh", "shared")

            reader = ExecCache(shared, user)
            cached = reader.find("info", "coreutils", "manual", "zh")

        self.assertEqual(cached, "shared")

    def test_translate_cached_text_uses_cache_without_model_call(self) -> None:
        with temporary_home() as home:
            shared = Path(home) / "shared.db"
            write_config(home, shared)
            cache = ExecCache(shared)
            cache.store_user("man", "git", "manual", "zh", "cached")

            with patch("hymt.exec_cache.translate_text", new_callable=AsyncMock) as tx:
                translated = asyncio.run(
                    translate_cached_text("man", "git", "manual", "zh", HotConfig())
                )

        self.assertEqual(translated, "cached")
        tx.assert_not_awaited()

    def test_translate_cached_text_uses_effective_target_for_cache_key(self) -> None:
        with temporary_home() as home:
            shared = Path(home) / "shared.db"
            write_config(home, shared)
            cache = ExecCache(shared)
            cache.store_user("man", "git", "中文段落。", "en", "cached")

            with (
                patch("hymt.language._load_langdetect", return_value=FakeDetector()),
                patch("hymt.exec_cache.translate_text", new_callable=AsyncMock) as tx,
            ):
                translated = asyncio.run(
                    translate_cached_text(
                        "man",
                        "git",
                        "中文段落。",
                        "zh",
                        HotConfig(),
                        explicit_target=False,
                    )
                )

        self.assertEqual(translated, "cached")
        tx.assert_not_awaited()

    def test_translate_cached_text_stores_live_translation_in_user_cache(self) -> None:
        with temporary_home() as home:
            shared = Path(home) / "shared.db"
            write_config(home, shared)

            with patch("hymt.exec_cache.translate_text", side_effect=fake_translate):
                translated = asyncio.run(
                    translate_cached_text("man", "git", "manual", "zh", HotConfig())
                )

            cached = ExecCache(shared).find("man", "git", "manual", "zh")

        self.assertEqual(translated, "ZH:manual")
        self.assertEqual(cached, "ZH:manual")


async def fake_translate(
    text: str,
    target_lang: str,
    config: HotConfig,
    template_type: TemplateType,
    *,
    stream: bool | None = None,
) -> str:
    del target_lang, config, template_type, stream
    return f"ZH:{text}"


def write_config(home: str, shared: Path) -> None:
    config_path = Path(home) / ".config" / "hymt" / "config.toml"
    config_path.parent.mkdir(parents=True, exist_ok=True)
    config_path.write_text(
        f"""
[exec]
shared_cache_path = "{shared}"
""",
        encoding="utf-8",
    )


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


class FakeDetector:
    def detect(self, text: str) -> str:
        if any("\u4e00" <= char <= "\u9fff" for char in text):
            return "zh"
        return "en"


if __name__ == "__main__":
    unittest.main()
