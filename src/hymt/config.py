from __future__ import annotations

from pathlib import Path
from threading import RLock
from typing import Any
import tomllib


DEFAULT_CONFIG = """[endpoint]
url = "http://127.0.0.1:8401/v1"
api_key = ""
model = ""

[translation]
context_window = 4096
max_output_tokens = 4096
concurrency = 1
config_version = 1
timeout = 600

[inference]
temperature = 0.7
top_p = 0.6
top_k = 20
repetition_penalty = 1.05
"""


class ConfigError(ValueError):
    pass


def config_path() -> Path:
    return Path.home() / ".config" / "hymt" / "config.toml"


def show() -> str:
    config = HotConfig()
    return config.show()


class HotConfig:
    def __init__(self, path: Path | str | None = None) -> None:
        self._path = Path(path).expanduser() if path is not None else config_path()
        self._lock = RLock()
        self._data: dict[str, Any] = {}
        self._mtime_ns: int | None = None
        self._ensure_exists()
        self._reload()

    @property
    def path(self) -> Path:
        return self._path

    @property
    def endpoint_url(self) -> str:
        return self._get_str("endpoint", "url", "http://127.0.0.1:8401/v1").rstrip("/")

    @property
    def api_key(self) -> str:
        return self._get_str("endpoint", "api_key", "")

    @property
    def model(self) -> str:
        return self._get_str("endpoint", "model", "")

    @property
    def context_window(self) -> int:
        return self._get_positive_int("translation", "context_window", 4096)

    @property
    def max_output_tokens(self) -> int:
        return self._get_positive_int("translation", "max_output_tokens", 4096)

    @property
    def concurrency(self) -> int:
        return self._get_positive_int("translation", "concurrency", 1)

    @property
    def config_version(self) -> int:
        return self._get_positive_int("translation", "config_version", 1)

    @property
    def timeout(self) -> float:
        return self._get_float("translation", "timeout", 600.0)

    @property
    def temperature(self) -> float:
        return self._get_float("inference", "temperature", 0.7)

    @property
    def top_p(self) -> float:
        return self._get_float("inference", "top_p", 0.6)

    @property
    def top_k(self) -> int:
        return self._get_positive_int("inference", "top_k", 20)

    @property
    def repetition_penalty(self) -> float:
        return self._get_float("inference", "repetition_penalty", 1.05)

    def maybe_reload(self) -> bool:
        with self._lock:
            current_mtime = self._stat_mtime()
            if current_mtime == self._mtime_ns:
                return False
            self._reload_unlocked()
            return True

    def show(self) -> str:
        self.maybe_reload()
        return self._path.read_text(encoding="utf-8")

    def _ensure_exists(self) -> None:
        if self._path.exists():
            return
        self._path.parent.mkdir(parents=True, exist_ok=True)
        self._path.write_text(DEFAULT_CONFIG, encoding="utf-8")

    def _reload(self) -> None:
        with self._lock:
            self._reload_unlocked()

    def _reload_unlocked(self) -> None:
        raw = self._path.read_bytes()
        try:
            data = tomllib.loads(raw.decode("utf-8"))
        except (tomllib.TOMLDecodeError, UnicodeDecodeError) as exc:
            raise ConfigError(f"Invalid config file: {self._path}") from exc
        mtime_ns = self._stat_mtime()
        self._data = data
        self._mtime_ns = mtime_ns

    def _stat_mtime(self) -> int:
        return self._path.stat().st_mtime_ns

    def _get_section(self, name: str) -> dict[str, Any]:
        section = self._data.get(name)
        if isinstance(section, dict):
            return section
        return {}

    def _get_str(self, section_name: str, key: str, default: str) -> str:
        with self._lock:
            value = self._get_section(section_name).get(key, default)
        return value if isinstance(value, str) else default

    def _get_positive_int(self, section_name: str, key: str, default: int) -> int:
        with self._lock:
            value = self._get_section(section_name).get(key, default)
        if isinstance(value, int) and not isinstance(value, bool) and value > 0:
            return value
        return default

    def _get_float(self, section_name: str, key: str, default: float) -> float:
        with self._lock:
            value = self._get_section(section_name).get(key, default)
        if isinstance(value, bool):
            return default
        if isinstance(value, int | float):
            return float(value)
        return default
