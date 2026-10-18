"""Installer contracts against the actual Just recipe and an isolated systemctl."""

import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest

__all__ = ["InstallerContracts"]

_ROOT = Path(__file__).resolve().parents[2]
_UNIT = "hy-mt2-cache.service"
_ACTIONS = ["daemon-reload", "enable", "restart"]
_SYSTEMCTL = r'''
import json, os, pathlib, sys
root = pathlib.Path(os.environ["INSTALL_FIXTURE"])
state_path = root / "state.json"
state = json.loads(state_path.read_text())
args = sys.argv[1:]
assert args == ["--user", "daemon-reload"] or (len(args) == 3 and args[0] == "--user" and args[1] in ("enable", "restart", "try-restart") and args[2] == "hy-mt2-cache.service"), args
action = args[1]
state["calls"].append(action)
if action != os.environ.get("FAIL_ACTION"):
    if action == "enable":
        state["enabled"] = True
    elif action == "restart" or (action == "try-restart" and state["active"]):
        home = pathlib.Path(os.environ["HOME"])
        source = pathlib.Path(os.environ["INSTALL_SOURCE"])
        assert (home / ".local/share/hymt/inference-cache.py").read_bytes() == (source / "scripts/hymt-inference-cache.py").read_bytes()
        assert (home / ".config/systemd/user/hy-mt2-cache.service").read_bytes() == (source / "services/hy-mt2-cache.service").read_bytes()
        state["active"] = True
        state["generation"] += 1
state_path.write_text(json.dumps(state))
sys.exit(42 if action == os.environ.get("FAIL_ACTION") else 0)
'''


class InstallerContracts(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.home = self.root / "home"
        self.home.mkdir(mode=0o700)
        self.bin = self.root / "bin"
        self.bin.mkdir(mode=0o700)
        systemctl = self.bin / "systemctl"
        systemctl.write_text(f"#!{sys.executable}\n" + _SYSTEMCTL)
        systemctl.chmod(0o700)
        self.just = shutil.which("just")
        self.assertIsNotNone(self.just)
        self.sentinels = {
            ".local/state/hymt/inference-cache.sqlite3": b"existing cache database",
            ".local/state/hymt/inference-cache.sqlite3.guard": b"\0",
            ".config/hymt/config.toml": b"existing user configuration",
            ".config/systemd/user/hy-mt2-quality.service": b"quality model unit",
            ".config/systemd/user/hy-mt2-throughput.service": b"throughput model unit",
        }
        for name, content in self.sentinels.items():
            path = self.home / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)

    def _install(self, *, active: bool = False, fail: str = "") -> tuple[subprocess.CompletedProcess[str], dict]:
        state_path = self.root / "state.json"
        state_path.write_text(json.dumps({"calls": [], "enabled": False, "active": active, "generation": int(active)}))
        env = os.environ.copy()
        env.update(HOME=str(self.home), PATH=str(self.bin) + os.pathsep + env["PATH"],
                   INSTALL_FIXTURE=str(self.root), INSTALL_SOURCE=str(_ROOT), FAIL_ACTION=fail,
                   TMPDIR=str(self.root), TMP=str(self.root), TEMP=str(self.root), JUST_TEMPDIR=str(self.root))
        result = subprocess.run([self.just, "install-inference-cache"], cwd=_ROOT, env=env,
                                capture_output=True, text=True, timeout=10)
        return result, json.loads(state_path.read_text())

    def _assert_preserved(self) -> None:
        for name, content in self.sentinels.items():
            self.assertEqual(content, (self.home / name).read_bytes(), name)
        self.assertEqual([], list(self.home.rglob(".inference-cache.*")))
        self.assertEqual([], list(self.home.rglob(".hy-mt2-cache.*")))

    def _assert_installed(self) -> None:
        for source, destination in (
            ("scripts/hymt-inference-cache.py", ".local/share/hymt/inference-cache.py"),
            (f"services/{_UNIT}", f".config/systemd/user/{_UNIT}"),
        ):
            path = self.home / destination
            self.assertEqual((_ROOT / source).read_bytes(), path.read_bytes())
            self.assertEqual(0o600, stat.S_IMODE(path.stat().st_mode))

    def test_install_enables_and_starts_inactive_cache(self) -> None:
        for existing in (False, True):
            with self.subTest(existing=existing):
                if existing:
                    (self.home / ".local/share/hymt/inference-cache.py").write_text("stale script")
                result, state = self._install()
                self.assertEqual(0, result.returncode, result.stderr)
                self.assertEqual(_ACTIONS, state["calls"])
                self.assertTrue(state["enabled"])
                self.assertTrue(state["active"])
                self.assertEqual(1, state["generation"])
                self._assert_installed()
                self._assert_preserved()

    def test_install_refreshes_active_cache_without_touching_data(self) -> None:
        (self.home / ".local/share/hymt").mkdir(parents=True, exist_ok=True)
        (self.home / ".local/share/hymt/inference-cache.py").write_text("stale script")
        (self.home / f".config/systemd/user/{_UNIT}").write_text("stale unit")
        result, state = self._install(active=True)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertEqual(_ACTIONS, state["calls"])
        self.assertTrue(state["enabled"])
        self.assertTrue(state["active"])
        self.assertEqual(2, state["generation"])
        self._assert_installed()
        self._assert_preserved()

    def test_systemctl_failures_propagate(self) -> None:
        for index, action in enumerate(_ACTIONS):
            with self.subTest(action=action):
                result, state = self._install(fail=action)
                self.assertNotEqual(0, result.returncode)
                self.assertEqual(_ACTIONS[:index + 1], state["calls"])
                self.assertFalse(state["active"])
                self._assert_preserved()


if __name__ == "__main__":
    unittest.main()
