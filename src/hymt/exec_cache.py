from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
import hashlib
import sqlite3

from hymt.config import HotConfig
from hymt.language import resolve_target_language
from hymt.templates import TemplateType
from hymt.translate import translate_text


SCHEMA = """
CREATE TABLE IF NOT EXISTS exec_cache (
    command TEXT NOT NULL,
    subcommand TEXT NOT NULL,
    output_hash TEXT NOT NULL,
    target_lang TEXT NOT NULL,
    source_text TEXT NOT NULL,
    translated_text TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (command, subcommand, output_hash, target_lang)
);
"""


@dataclass(frozen=True)
class ExecCacheKey:
    command: str
    subcommand: str
    output_hash: str
    target_lang: str


def user_exec_cache_path() -> Path:
    return Path.home() / ".cache" / "hymt" / "exec-cache.db"


class ExecCache:
    def __init__(
        self, shared_path: Path | str, user_path: Path | str | None = None
    ) -> None:
        self._shared_path = Path(shared_path).expanduser()
        self._user_path = (
            Path(user_path).expanduser()
            if user_path is not None
            else user_exec_cache_path()
        )

    @property
    def user_path(self) -> Path:
        return self._user_path

    @property
    def shared_path(self) -> Path:
        return self._shared_path

    def find(
        self, command: str, subcommand: str, source_text: str, target_lang: str
    ) -> str | None:
        key = build_exec_cache_key(command, subcommand, source_text, target_lang)
        cached = self._find_in_user(key)
        if cached is not None:
            return cached
        return self._find_in_shared(key)

    def store_user(
        self,
        command: str,
        subcommand: str,
        source_text: str,
        target_lang: str,
        translated_text: str,
    ) -> None:
        self._store(
            self._connect_user(create=True),
            build_exec_cache_key(command, subcommand, source_text, target_lang),
            source_text,
            translated_text,
        )
        self._user_path.chmod(0o600)

    def store_shared(
        self,
        command: str,
        subcommand: str,
        source_text: str,
        target_lang: str,
        translated_text: str,
    ) -> None:
        self._store(
            self._connect_shared(create=True),
            build_exec_cache_key(command, subcommand, source_text, target_lang),
            source_text,
            translated_text,
        )
        self._shared_path.chmod(0o644)

    def _find_in_user(self, key: ExecCacheKey) -> str | None:
        if not self._user_path.exists():
            return None
        connection = self._connect_user(create=False)
        try:
            return self._find(connection, key)
        finally:
            connection.close()

    def _find_in_shared(self, key: ExecCacheKey) -> str | None:
        if not self._shared_path.exists():
            return None
        try:
            connection = sqlite3.connect(f"file:{self._shared_path}?mode=ro", uri=True)
        except sqlite3.Error:
            return None
        connection.row_factory = sqlite3.Row
        try:
            return self._find(connection, key)
        except sqlite3.Error:
            return None
        finally:
            connection.close()

    def _find(self, connection: sqlite3.Connection, key: ExecCacheKey) -> str | None:
        self._ensure_schema(connection)
        row = connection.execute(
            """
            SELECT translated_text
            FROM exec_cache
            WHERE command = ?
              AND subcommand = ?
              AND output_hash = ?
              AND target_lang = ?
            LIMIT 1
            """,
            (key.command, key.subcommand, key.output_hash, key.target_lang),
        ).fetchone()
        if row is None:
            return None
        return str(row["translated_text"])

    def _store(
        self,
        connection: sqlite3.Connection,
        key: ExecCacheKey,
        source_text: str,
        translated_text: str,
    ) -> None:
        try:
            self._ensure_schema(connection)
            connection.execute(
                """
                INSERT INTO exec_cache (
                    command,
                    subcommand,
                    output_hash,
                    target_lang,
                    source_text,
                    translated_text,
                    created_at
                )
                VALUES (?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(command, subcommand, output_hash, target_lang)
                DO UPDATE SET
                    source_text = excluded.source_text,
                    translated_text = excluded.translated_text,
                    created_at = excluded.created_at
                """,
                (
                    key.command,
                    key.subcommand,
                    key.output_hash,
                    key.target_lang,
                    source_text,
                    translated_text,
                    datetime.now(timezone.utc).isoformat(timespec="seconds"),
                ),
            )
            connection.commit()
        finally:
            connection.close()

    def _connect_user(self, *, create: bool) -> sqlite3.Connection:
        if create:
            self._user_path.parent.mkdir(parents=True, exist_ok=True)
        connection = sqlite3.connect(str(self._user_path))
        connection.row_factory = sqlite3.Row
        return connection

    def _connect_shared(self, *, create: bool) -> sqlite3.Connection:
        if create:
            self._shared_path.parent.mkdir(parents=True, exist_ok=True)
        connection = sqlite3.connect(str(self._shared_path))
        connection.row_factory = sqlite3.Row
        return connection

    def _ensure_schema(self, connection: sqlite3.Connection) -> None:
        connection.executescript(SCHEMA)
        connection.commit()


def build_exec_cache_key(
    command: str, subcommand: str, source_text: str, target_lang: str
) -> ExecCacheKey:
    return ExecCacheKey(
        command=command,
        subcommand=subcommand,
        output_hash=hash_output_text(source_text),
        target_lang=target_lang,
    )


def hash_output_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


async def translate_cached_text(
    command: str,
    subcommand: str,
    text: str,
    target_lang: str,
    config: HotConfig,
    *,
    refresh: bool = False,
    explicit_target: bool = True,
) -> str:
    effective_target_lang = resolve_target_language(
        text, target_lang, config, explicit_target=explicit_target
    )
    cache = ExecCache(config.exec_shared_cache_path)
    if not refresh:
        cached = cache.find(command, subcommand, text, effective_target_lang)
        if cached is not None:
            return cached
    translated = await _translate_for_cache(
        text, effective_target_lang, config, refresh=refresh
    )
    cache.store_user(command, subcommand, text, effective_target_lang, translated)
    return translated


async def _translate_for_cache(
    text: str, target_lang: str, config: HotConfig, *, refresh: bool
) -> str:
    if refresh:
        return await translate_text(
            text,
            target_lang,
            config,
            TemplateType.DEFAULT,
            stream=False,
            cache_bust=datetime.now(timezone.utc).isoformat(timespec="seconds"),
        )
    return await translate_text(
        text,
        target_lang,
        config,
        TemplateType.DEFAULT,
        stream=False,
    )
