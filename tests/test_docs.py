from __future__ import annotations

import unittest
from unittest.mock import patch

from click.testing import CliRunner

from hymt.cli import main
from hymt.config import HotConfig
from hymt.docs import show_translated_man
from hymt.templates import TemplateType


class DocsCommandTests(unittest.TestCase):
    def test_man_translates_captured_page_and_sends_to_pager(self) -> None:
        paged: list[str] = []

        with (
            patch("hymt.docs._capture_man", return_value="git manual"),
            patch("hymt.docs.translate_text", side_effect=fake_translate),
            patch(
                "hymt.docs._page_text", side_effect=lambda text: paged.append(text) or 0
            ),
        ):
            returncode = show_translated_man(("git",), "zh", HotConfig())

        self.assertEqual(returncode, 0)
        self.assertEqual(paged, ["ZH:git manual"])

    def test_man_original_passes_through_to_system_man(self) -> None:
        with patch("hymt.docs.subprocess.call", return_value=3) as call:
            returncode = show_translated_man(("git",), "zh", HotConfig(), original=True)

        self.assertEqual(returncode, 3)
        call.assert_called_once_with(["man", "git"])

    def test_man_refresh_uses_new_cache_identity_without_changing_prompt(self) -> None:
        captured_kwargs: list[dict[str, object]] = []

        async def capture_translate(
            text: str,
            target_lang: str,
            config: HotConfig,
            template_type: TemplateType,
            **kwargs: object,
        ) -> str:
            del target_lang, config, template_type
            captured_kwargs.append(kwargs)
            return f"ZH:{text}"

        with (
            patch("hymt.docs._capture_man", return_value="git manual"),
            patch("hymt.docs.translate_text", side_effect=capture_translate),
            patch("hymt.docs._page_text", return_value=0),
        ):
            show_translated_man(("git",), "zh", HotConfig(), refresh=True)

        self.assertEqual(captured_kwargs[0]["stream"], False)
        self.assertIn("cache_bust", captured_kwargs[0])

    def test_cli_preserves_man_apropos_arguments(self) -> None:
        runner = CliRunner()
        with patch("hymt.cli.show_translated_man", return_value=0) as command:
            result = runner.invoke(main, ["man", "-k", "file system"])

        self.assertEqual(result.exit_code, 0)
        command.assert_called_once()
        self.assertEqual(command.call_args.args[0], ("-k", "file system"))


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


if __name__ == "__main__":
    unittest.main()
