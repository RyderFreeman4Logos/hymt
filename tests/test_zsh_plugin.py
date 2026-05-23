from __future__ import annotations

import os
import tempfile
import unittest

from click.testing import CliRunner

from hymt.cli import main
from hymt.config import HotConfig
from hymt.zsh_plugin import install_zsh_plugin


class ZshPluginTests(unittest.TestCase):
    def test_install_writes_plugin_and_zshrc_source_line(self) -> None:
        with temporary_home() as home:
            result = install_zsh_plugin(HotConfig())

            plugin_text = result.plugin_path.read_text(encoding="utf-8")
            zshrc_text = result.zshrc_path.read_text(encoding="utf-8")

        self.assertTrue(str(result.plugin_path).startswith(home))
        self.assertIn("t() {", plugin_text)
        self.assertIn("[[ -o interactive ]]", plugin_text)
        self.assertIn("[[ -t 1 && -t 2 ]]", plugin_text)
        self.assertIn("_hymt_has_agent_env", plugin_text)
        self.assertIn("CODEX_SESSION_ID", plugin_text)
        self.assertIn("OPENCODE_SESSION", plugin_text)
        self.assertIn("_hymt_is_agent_child", plugin_text)
        self.assertIn("_hymt_command_blocked", plugin_text)
        self.assertIn('${1:t}" == "hymt"', plugin_text)
        self.assertIn("_hymt_inside_script", plugin_text)
        self.assertNotIn("preexec", plugin_text)
        self.assertIn('command hymt exec -- "$@"', plugin_text)
        self.assertIn('"$@"', plugin_text)
        self.assertIn(result.source_line, zshrc_text)

    def test_install_requires_update_for_existing_plugin(self) -> None:
        with temporary_home():
            config = HotConfig()
            install_zsh_plugin(config)

            with self.assertRaises(FileExistsError):
                install_zsh_plugin(config)

            result = install_zsh_plugin(config, update=True)

        self.assertTrue(result.updated_plugin)
        self.assertFalse(result.updated_zshrc)

    def test_cli_exec_install_uses_home_config(self) -> None:
        runner = CliRunner()
        with temporary_home() as home:
            result = runner.invoke(main, ["exec", "install"])
            zshrc_exists = os.path.exists(os.path.join(home, ".zshrc"))

        self.assertEqual(result.exit_code, 0)
        self.assertIn("Installed", result.output)
        self.assertIn(".local/share/hymt/hymt-exec.zsh", result.output)
        self.assertTrue(zshrc_exists)


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
