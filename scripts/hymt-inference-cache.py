"""Shared plaintext llama.cpp response cache. Run behind the authenticating gateway."""

import argparse
import errno
import fcntl
import hashlib
import http.client
import ipaddress
import json
import os
import select
import signal
import socket
import sqlite3
import ssl
import stat
import threading
import time
from collections.abc import Iterator
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from socketserver import ThreadingMixIn
from urllib.parse import urlsplit

__all__ = ["CacheServer", "main"]
_SCHEMA = "hymt-inference-cache-v2"
_PROMPT_SCHEMA = "llama.cpp-chat-template-v1"
_HOP_HEADERS = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade", "content-length", "x-hymt-cache"}
_HEADER_LIMIT = 1024 * 1024
_TOKEN_CHARS = frozenset("!#$%&'*+-.^_`|~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz")


def _valid_headers(headers: object) -> bool:
    if not isinstance(headers, list) or len(headers) > 100:
        return False
    for item in headers:
        if not isinstance(item, (list, tuple)) or len(item) != 2:
            return False
        name, value = item
        if not isinstance(name, str) or not name or any(char not in _TOKEN_CHARS for char in name):
            return False
        if not isinstance(value, str) or any(ord(char) > 255 or ord(char) < 32 and char != "\t" or ord(char) == 127 for char in value):
            return False
    return True


def _cacheable_headers(headers: list[tuple[str, str]] | list[list[str]]) -> bool:
    if not _valid_headers(headers) or len(json.dumps(headers).encode()) > _HEADER_LIMIT:
        return False
    connection_tokens = {token.strip().lower() for name, value in headers if name.lower() == "connection" for token in value.split(",")}
    effective = [(name.lower(), value) for name, value in headers if name.lower() not in _HOP_HEADERS | connection_tokens]
    return sum(name == "content-type" for name, _ in effective) == 1 and all(name != "content-encoding" or value.strip().lower() == "identity" for name, value in effective)


class _Invalid(ValueError):
    """Unusable JSON, response or upstream identity."""


class _NoPlanning(_Invalid):
    """A readable store with no retained planning row, rather than unknown prior authority."""


class _Deadline(TimeoutError):
    """Local request or admission budget exhausted; not an upstream outage."""


class _Unavailable(OSError):
    """A classified upstream transport outage, never local admission or bad metadata."""


class _Number(str):
    """Retain exact JSON numeric tokens instead of rounding through binary floats."""


def _pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise _Invalid("duplicate JSON key")
        result[key] = value
    return result


def _constant(_value: str) -> object:
    raise _Invalid("nonfinite JSON number")


def _load(raw: bytes) -> object:
    return json.loads(raw.decode("utf-8"), object_pairs_hook=_pairs, parse_int=_Number, parse_float=_Number, parse_constant=_constant)


def _canonical(value: object) -> str:
    if isinstance(value, _Number):
        return str(value)
    if isinstance(value, dict):
        return "{" + ",".join(json.dumps(key, ensure_ascii=True) + ":" + _canonical(value[key]) for key in sorted(value)) + "}"
    if isinstance(value, list):
        return "[" + ",".join(_canonical(item) for item in value) + "]"
    return json.dumps(value, ensure_ascii=True, allow_nan=False, separators=(",", ":"))


def _complete(raw: bytes, content_type: str) -> bool:
    try:
        if content_type.split(";", 1)[0].strip().lower() == "application/json":
            value = _load(raw)
            if not isinstance(value, dict) or "error" in value:
                return False
            choices = value.get("choices")
            return isinstance(choices, list) and bool(choices) and all(
                isinstance(choice, dict) and choice.get("finish_reason") == "stop"
                and isinstance(choice.get("message"), dict)
                and type(choice["message"].get("content")) is str
                and isinstance(choice.get("index"), _Number) and str(choice["index"]).isascii() and str(choice["index"]).isdigit()
                and bool(choice["message"]["content"].strip()) for choice in choices)
        if content_type.split(";", 1)[0].strip().lower() != "text/event-stream":
            return False
        text = raw.decode("utf-8").replace("\r\n", "\n")
        if not text.endswith("\n\n"):
            return False
        contents: dict[str, str] = {}
        finished: set[str] = set()
        observed: set[str] = set()
        done = False
        for frame in text.split("\n\n"):
            data = []
            for line in frame.split("\n"):
                if not line or line.startswith(":"):
                    continue
                field, sep, value = line.partition(":")
                if not sep or field not in {"data", "event", "id", "retry"}:
                    return False
                if field == "data":
                    data.append(value.removeprefix(" "))
            if not data:
                continue
            if done:
                return False
            payload = "\n".join(data)
            if payload == "[DONE]":
                done = True
                continue
            value = _load(payload.encode())
            if not isinstance(value, dict) or "error" in value or not isinstance(value.get("choices"), list):
                return False
            for choice in value["choices"]:
                if not isinstance(choice, dict) or not isinstance(choice.get("index"), _Number):
                    return False
                index = str(choice["index"])
                if not index.isascii() or not index.isdigit():
                    return False
                observed.add(index)
                delta = choice.get("delta")
                if not isinstance(delta, dict):
                    return False
                content = delta.get("content")
                if content is not None:
                    if type(content) is not str or index in finished:
                        return False
                    contents[index] = contents.get(index, "") + content
                reason = choice.get("finish_reason")
                if reason is not None:
                    if reason != "stop" or index in finished:
                        return False
                    finished.add(index)
        return done and bool(contents) and set(contents) == finished == observed and all(value.strip() for value in contents.values())
    except (ValueError, UnicodeError, RecursionError, TypeError):
        return False


class _Budget:
    def __init__(self, timeout: float, parent: "_Budget | None" = None) -> None:
        self.parent = parent
        self.end = time.monotonic() + timeout
        self.lock = threading.Lock()
        self.sockets: list[socket.socket] = []
        self.expired = False
        self.timer = threading.Timer(timeout, self.expire)
        self.timer.start()

    def remaining(self) -> float:
        remaining = self.end - time.monotonic()
        if remaining <= 0 or self.expired:
            raise _Deadline("request deadline")
        return remaining

    def add(self, sock: socket.socket) -> None:
        if self.parent is not None:
            self.parent.add(sock)
        with self.lock:
            if self.expired:
                sock.close()
                raise _Deadline("request deadline")
            self.sockets.append(sock)
            sock.settimeout(self.remaining())

    def expire(self) -> None:
        with self.lock:
            self.expired = True
            for sock in self.sockets:
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                sock.close()

    def close(self) -> None:
        self.timer.cancel()
        self.timer.join()


class _Store:
    def __init__(self, path: Path) -> None:
        self.path = path
        self.ready = False
        try:
            path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
            try:
                stat = os.fstat(fd)
                if stat.st_uid != os.getuid() or stat.st_mode & 0o077:
                    raise _Invalid("cache database must be private")
            finally:
                os.close(fd)
            with self.connect() as conn:
                conn.execute("PRAGMA journal_mode=WAL")
                conn.execute("CREATE TABLE IF NOT EXISTS epoch (id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL)")
                if conn.execute("SELECT value FROM epoch WHERE id=1").fetchone() is None:
                    conn.execute("INSERT OR IGNORE INTO epoch VALUES (1, 0)")
                conn.execute("CREATE TABLE IF NOT EXISTS entries (key TEXT PRIMARY KEY, epoch INTEGER NOT NULL, request BLOB NOT NULL, canonical TEXT NOT NULL, identity TEXT NOT NULL, response BLOB NOT NULL, headers TEXT NOT NULL, digest TEXT NOT NULL, complete INTEGER NOT NULL CHECK(complete=1))")
                conn.execute("CREATE TABLE IF NOT EXISTS planning (id INTEGER PRIMARY KEY CHECK(id=1), epoch INTEGER NOT NULL, identity TEXT NOT NULL, observed INTEGER NOT NULL, token_request TEXT, token_response BLOB, token_digest TEXT)")
            self.ready = True
        except (OSError, sqlite3.Error, _Invalid):
            pass

    @contextmanager
    def connect(self) -> Iterator[sqlite3.Connection]:
        connection = sqlite3.connect(self.path, timeout=0.1)
        try:
            with connection:
                yield connection
        finally:
            connection.close()

    def epoch(self) -> int:
        if not self.ready:
            raise _Invalid("cache database unavailable")
        with self.connect() as conn:
            row = conn.execute("SELECT value FROM epoch WHERE id=1").fetchone()
            if row is None or not isinstance(row[0], int):
                raise _Invalid("invalid database epoch")
            return row[0]

    def read(self, key: str, epoch: int, canonical: str, identity: str, request_limit: int, response_limit: int) -> tuple[list[tuple[str, str]], bytes] | None:
        with self.connect() as conn:
            row = conn.execute("""SELECT response, headers, digest FROM entries WHERE key=? AND epoch=?
                AND typeof(epoch)='integer' AND typeof(request)='blob' AND length(request)<=?
                AND typeof(canonical)='text' AND canonical=? AND length(CAST(canonical AS BLOB))<=?
                AND typeof(identity)='text' AND identity=? AND length(CAST(identity AS BLOB))<=?
                AND typeof(response)='blob' AND length(response)<=?
                AND typeof(headers)='text' AND length(CAST(headers AS BLOB))<=?
                AND typeof(digest)='text' AND length(CAST(digest AS BLOB))=64
                AND typeof(complete)='integer' AND complete=1""",
                (key, epoch, request_limit, canonical, len(canonical.encode()), identity, len(identity.encode()), response_limit, _HEADER_LIMIT)).fetchone()
        if row is None:
            return None
        raw, serialized, digest = row
        if hashlib.sha256(raw).hexdigest() != digest:
            return None
        headers = json.loads(serialized)
        if not _cacheable_headers(headers):
            return None
        content_type = next((value for name, value in headers if name.lower() == "content-type"), "")
        if not _complete(raw, content_type):
            return None
        return headers, raw

    def write(self, key: str, epoch: int, request: bytes, canonical: str, identity: str, headers: list[tuple[str, str]], raw: bytes, request_limit: int, response_limit: int) -> bool:
        if len(request) > request_limit or len(raw) > response_limit or not _cacheable_headers(headers):
            return False
        with self.connect() as conn:
            conn.execute("BEGIN IMMEDIATE")
            if conn.execute("SELECT value FROM epoch WHERE id=1").fetchone()[0] != epoch:
                return False
            conn.execute("INSERT OR REPLACE INTO entries VALUES (?,?,?,?,?,?,?,?,1)", (key, epoch, request, canonical, identity, raw, json.dumps(headers), hashlib.sha256(raw).hexdigest()))
        return True

    def activate(self, identity: str) -> None:
        if len(identity.encode()) > 1024 * 1024:
            raise _Invalid("planning evidence too large")
        if not self.ready:
            raise _Invalid("cache database unavailable")
        with self.connect() as conn:
            conn.execute("BEGIN IMMEDIATE")
            old = conn.execute("SELECT identity=? AND typeof(epoch)='integer' AND epoch=(SELECT value FROM epoch WHERE id=1) FROM planning WHERE id=1", (identity,)).fetchone()
            if old is not None and not old[0]:
                conn.execute("UPDATE epoch SET value=value+1 WHERE id=1")
                conn.execute("DELETE FROM entries")
                conn.execute("DELETE FROM planning")
            epoch = conn.execute("SELECT value FROM epoch WHERE id=1").fetchone()[0]
            conn.execute("INSERT INTO planning (id,epoch,identity,observed) VALUES (1,?,?,?) ON CONFLICT(id) DO UPDATE SET observed=excluded.observed", (epoch, identity, int(time.time())))

    def planning(self) -> tuple[int, str, int, str | None, bytes | None, str | None]:
        if not self.ready:
            raise _Invalid("cache database unavailable")
        with self.connect() as conn:
            row = conn.execute("""SELECT p.epoch,p.identity,p.observed,p.token_request,p.token_response,p.token_digest
                FROM planning p JOIN epoch e ON e.id=1 AND e.value=p.epoch WHERE p.id=1
                AND typeof(p.epoch)='integer' AND typeof(p.identity)='text' AND length(CAST(p.identity AS BLOB))<=1048576
                AND typeof(p.observed)='integer' AND p.observed>0
                AND (p.token_request IS NULL OR (typeof(p.token_request)='text' AND length(CAST(p.token_request AS BLOB))<=4096))
                AND (p.token_response IS NULL OR (typeof(p.token_response)='blob' AND length(p.token_response)<=4096))
                AND (p.token_digest IS NULL OR (typeof(p.token_digest)='text' AND length(p.token_digest)=64))""").fetchone()
            if row is None and conn.execute("SELECT 1 FROM planning WHERE id=1").fetchone() is None:
                raise _NoPlanning("no retained active planning evidence")
        if row is None or row[2] > int(time.time()) + 1:
            raise _Invalid("no retained active planning evidence")
        return row

    def retain_token(self, epoch: int, identity: str, request: str, response: bytes) -> None:
        if len(request.encode()) > 4096 or len(response) > 4096:
            return
        with self.connect() as conn:
            conn.execute("UPDATE planning SET token_request=?,token_response=?,token_digest=? WHERE id=1 AND epoch=? AND identity=? AND epoch=(SELECT value FROM epoch WHERE id=1)", (request, response, hashlib.sha256(response).hexdigest(), epoch, identity))

    def clear(self) -> None:
        if not self.ready:
            raise _Invalid("cache database unavailable")
        with self.connect() as conn:
            conn.execute("BEGIN IMMEDIATE")
            conn.execute("UPDATE epoch SET value=value+1 WHERE id=1")
            conn.execute("DELETE FROM entries")
            conn.execute("DELETE FROM planning")


class _Flight:
    def __init__(self) -> None:
        self.done = threading.Event()
        self.reply: tuple[int, list[tuple[str, str]], bytes] | None = None
        self.admitted = False


class CacheServer(ThreadingMixIn, HTTPServer):
    """Bounded HTTP cache; callers must enforce credentials before shared hits."""

    daemon_threads = False
    block_on_close = True

    def __init__(self, address: tuple[str, int], upstream: str, database: Path, *, timeout: float = 120, max_clients: int = 16, max_upstream: int = 4, request_limit: int = 2 * 1024 * 1024, response_limit: int = 8 * 1024 * 1024) -> None:
        parsed = urlsplit(upstream)
        if parsed.scheme not in {"http", "https"} or not parsed.hostname or parsed.username or parsed.password or parsed.path not in {"", "/"} or parsed.query or parsed.fragment:
            raise _Invalid("upstream must be an HTTP origin without credentials")
        # shortcut: numeric upstream addresses bound connection setup, add bounded DNS only when required.
        ipaddress.ip_address(parsed.hostname)
        if not 0 < timeout <= threading.TIMEOUT_MAX or min(max_clients, max_upstream, request_limit, response_limit) <= 0:
            raise _Invalid("limits must be positive")
        self.upstream = parsed
        self.request_timeout = timeout
        self.upstream_host = parsed.hostname
        self.request_limit = request_limit
        self.response_limit = response_limit
        self.clients = threading.BoundedSemaphore(max_clients)
        self.generations = threading.BoundedSemaphore(max_upstream)
        self.controls = threading.BoundedSemaphore(1)
        self.planning_lock = threading.RLock()
        self.planning_revoked = False
        self.planning_generation = 0
        self.observation_sequence = 0
        self.planning_observation = 0
        self.active_observations = 0
        self.guard_fd: int | None = None
        self.flight_lock = threading.Lock()
        self.flights: dict[str, _Flight] = {}
        self.closing = False
        self.budget_lock = threading.Lock()
        self.budgets: dict[socket.socket, _Budget] = {}
        try:
            database.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            guard = database.with_name(database.name + ".guard")
            flags = os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK
            created = True
            try:
                self.guard_fd = os.open(guard, flags | os.O_CREAT | os.O_EXCL, 0o600)
            except FileExistsError:
                self.guard_fd = os.open(guard, flags)
                created = False
            info = os.fstat(self.guard_fd)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_size > 1:
                raise _Invalid("cache guard must be a private one-byte regular file")
            fcntl.flock(self.guard_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            if not created:
                self.planning_revoked = os.pread(self.guard_fd, 1, 0) != b"0"
            # Write ahead of startup inspection and all identity-bearing network I/O.
            # The guard only denies; SQLite remains the sole positive authority.
            self._write_guard(b"1")
            self.store = _Store(database)
            try:
                previous = _load(self.store.planning()[1].encode())
            except _NoPlanning:
                pass
            except (sqlite3.Error, ValueError, TypeError, RecursionError):
                self.planning_revoked = True
            else:
                if not isinstance(previous, dict) or previous.get("upstream") != self.upstream.geturl():
                    self.planning_revoked = True
                    # A known origin switch must commit invalidation before listener startup.
                    self.store.clear()
            if not self.planning_revoked:
                self._write_guard(b"0")
            super().__init__(address, _Handler)
        except BaseException:
            self._close_guard()
            raise

    def _write_guard(self, value: bytes) -> None:
        if self.guard_fd is None or os.pwrite(self.guard_fd, value, 0) != 1:
            raise _Invalid("cache guard unavailable")

    def _close_guard(self) -> None:
        if self.guard_fd is not None:
            os.close(self.guard_fd)
            self.guard_fd = None

    def process_request(self, request: socket.socket | tuple[bytes, socket.socket], client_address: tuple[str, int]) -> None:
        if not isinstance(request, socket.socket):
            raise _Invalid("TCP socket required")
        if not self.clients.acquire(blocking=False):
            try:
                request.settimeout(0.1)
                request.sendall(b'HTTP/1.1 503 Service Unavailable\r\nX-Hymt-Cache: UNKNOWN\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            except OSError:
                pass
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except (OSError, RuntimeError):
            self.clients.release()
            self.shutdown_request(request)
            raise

    def process_request_thread(self, request: socket.socket | tuple[bytes, socket.socket], client_address: tuple[str, int]) -> None:
        if not isinstance(request, socket.socket):
            raise _Invalid("TCP socket required")
        budget = _Budget(self.request_timeout)
        with self.budget_lock:
            self.budgets[request] = budget
            if self.closing:
                budget.expire()
        try:
            budget.add(request)
            self.finish_request(request, client_address)
        except (OSError, http.client.HTTPException, ValueError, RecursionError):
            pass
        finally:
            budget.close()
            with self.budget_lock:
                self.budgets.pop(request, None)
            self.shutdown_request(request)
            self.clients.release()

    def server_close(self) -> None:
        with self.budget_lock:
            self.closing = True
            for budget in self.budgets.values():
                budget.expire()
        try:
            super().server_close()
        finally:
            self._close_guard()

    @contextmanager
    def upstream_response(self, method: str, path: str, raw: bytes | None, authorization: str | None, budget: _Budget) -> Iterator[http.client.HTTPResponse]:
        slots = self.controls if path in {"/props", "/v1/models", "/tokenize"} else self.generations
        if not slots.acquire(timeout=budget.remaining()):
            raise TimeoutError("upstream concurrency deadline")
        connection = None
        try:
            connection = http.client.HTTPConnection(self.upstream_host, self.upstream.port, timeout=budget.remaining())
            raw_socket = socket.socket(socket.AF_INET6 if ":" in self.upstream_host else socket.AF_INET, socket.SOCK_STREAM)
            connection.sock = raw_socket
            budget.add(raw_socket)
            raw_socket.connect((self.upstream_host, self.upstream.port or (443 if self.upstream.scheme == "https" else 80)))
            raw_socket.settimeout(budget.remaining())
            if self.upstream.scheme == "https":
                context = ssl.create_default_context()
                wrapped = context.wrap_socket(raw_socket, server_hostname=self.upstream_host, do_handshake_on_connect=False)
                connection.sock = wrapped
                budget.add(wrapped)
                wrapped.settimeout(budget.remaining())
                wrapped.do_handshake()
            headers = {"Accept-Encoding": "identity", "Content-Type": "application/json"}
            if authorization:
                headers["Authorization"] = authorization
            connection.request(method, path, body=raw, headers=headers)
            connection.sock.settimeout(budget.remaining())
            response = connection.getresponse()
            try:
                yield response
            finally:
                response.close()
        except (ssl.SSLError, _Deadline):
            raise
        except http.client.RemoteDisconnected as error:
            if budget.expired and (budget.parent is None or budget.parent.expired):
                raise
            raise _Unavailable("upstream peer closed before HTTP status") from error
        except http.client.HTTPException as error:
            if budget.expired and budget.parent is not None and not budget.parent.expired:
                raise _Unavailable("upstream probe transport deadline") from error
            raise
        except OSError as error:
            if budget.expired and budget.parent is not None and not budget.parent.expired:
                raise _Unavailable("upstream probe transport deadline") from error
            if isinstance(error, (ConnectionRefusedError, ConnectionResetError, ConnectionAbortedError, socket.timeout)) or error.errno in {errno.ENETUNREACH, errno.EHOSTUNREACH, errno.EPIPE}:
                raise _Unavailable("upstream transport unavailable") from error
            raise
        finally:
            if connection is not None:
                connection.close()
            slots.release()

    def observe(self, requested_model: object, authorization: str | None, budget: _Budget) -> str:
        observation = self.begin_observation()
        probe = None
        try:
            probe = _Budget(min(2.0, budget.remaining() / 4), budget)
            return self._observe(requested_model, authorization, probe, observation=observation)
        finally:
            if probe is not None:
                probe.close()
            self.end_observation()

    def begin_observation(self) -> int:
        with self.planning_lock:
            self._write_guard(b"1")
            self.active_observations += 1
            self.observation_sequence += 1
            return self.observation_sequence

    def end_observation(self) -> None:
        with self.planning_lock:
            self.active_observations -= 1
            if self.active_observations == 0 and not self.planning_revoked:
                try:
                    self._write_guard(b"0")
                except (OSError, ValueError):
                    self.planning_revoked = True
                    self.planning_generation += 1

    def revoke_observation(self, observation: int, *, clear: bool = True) -> None:
        with self.planning_lock:
            if observation < self.planning_observation:
                raise _Invalid("metadata observation was superseded")
            self.planning_observation = observation
            self.planning_revoked = True
            self.planning_generation += 1
            if clear:
                self.store.clear()

    def metadata(self, path: str, authorization: str | None, budget: _Budget) -> tuple[int, list[tuple[str, str]], bytes]:
        with self.upstream_response("GET", path, None, authorization, budget) as response:
            headers = response.getheaders()
            if not _valid_headers(headers):
                raise _Invalid("invalid upstream metadata headers")
            raw = b''
            read_body = True
            if response.status in {502, 503, 504}:
                try:
                    self.retained(None)
                except (sqlite3.Error, ValueError, TypeError, RecursionError):
                    pass
                else:
                    read_body = False  # A potential historical hit must not wait on an error body.
            if read_body:
                try:
                    raw = _read(response, budget, self.response_limit)
                except (OSError, http.client.HTTPException, ValueError):
                    if response.status == 200:
                        raise
                    # Preserve a known rejection even if its optional error body cannot be read.
            return response.status, headers, raw

    def _observe(self, requested_model: object, authorization: str | None, budget: _Budget, initial: tuple[str, tuple[int, list[tuple[str, str]], bytes] | None] | None = None, *, observation: int) -> str:
        values = {}
        retained = None
        unknown_prior = False
        try:
            retained = self.retained(None)
        except _NoPlanning:
            pass
        except (sqlite3.Error, ValueError, TypeError, RecursionError):
            unknown_prior = True
            # Unreadable prior authority must stay denied even if every probe is unavailable.
            self.revoke_observation(observation, clear=False)
        unavailable = False
        paths = ("/props", "/v1/models")
        if initial is not None:
            paths = (initial[0], *(path for path in paths if path != initial[0]))
        for path in paths:
            try:
                result = initial[1] if initial is not None and path == initial[0] else self.metadata(path, authorization, budget)
                if result is None:
                    raise _Unavailable("upstream metadata unavailable")
                status, headers, raw = result
                if status in {502, 503, 504}:
                    raise _Unavailable("upstream metadata unavailable")
                if status != 200:
                    raise _Invalid("upstream identity rejected")
                representation = [item for item in headers if item[0].lower() in {"content-type", "content-encoding", "connection"}]
                if not _valid_headers(headers) or not _cacheable_headers(representation) or len(raw) > 1024 * 1024:
                    raise _Invalid("invalid metadata representation")
                value = _load(raw)
            except _Unavailable:
                value = None
                unavailable = True
            values[path] = value
            if value is not None and unknown_prior:
                self.revoke_observation(observation)
                unknown_prior = False
            if value is not None and retained is not None:
                old = _load(retained[1].encode())
                props = values.get("/props") if values.get("/props") is not None else old["props"]
                models = values.get("/v1/models") if values.get("/v1/models") is not None else {"data": [old["selected_model"]]}
                try:
                    matches = self.identity(props, models, None) == retained[1]
                except (ValueError, TypeError, RecursionError):
                    matches = False
                if not matches:
                    # Revoke an observed contradiction before a later probe can fail, independent of caller alias.
                    self.revoke_observation(observation)
                    retained = None
            if unavailable:
                try:
                    budget.remaining()
                except _Deadline:
                    # A socket timeout can precede the child timer callback.
                    # Only a scoped probe may expire into historical replay;
                    # a genuine parent request deadline must still propagate.
                    if budget.parent is None:
                        raise
                    budget.parent.remaining()
                    break
        if unavailable:
            raise _Unavailable("upstream metadata unavailable")
        identity = self.identity(values["/props"], values["/v1/models"], None)
        with self.planning_lock:
            if observation < self.planning_observation:
                if self.retained(None)[1] != identity:
                    raise _Invalid("metadata observation was superseded")
                # Identical overlapping probes can share the current authority without republishing old evidence.
                return self.identity(values["/props"], values["/v1/models"], requested_model)
            self.planning_observation = observation
            # A failed revocation may leave old entries, even if the backend returns to that identity.
            if self.planning_revoked:
                self.store.clear()
            self.store.activate(identity)
            self.planning_revoked = False
        # A valid replacement model revokes old evidence even if this caller requested the old alias.
        return self.identity(values["/props"], values["/v1/models"], requested_model)

    def identity(self, props: object, models: object, requested_model: object) -> str:
        if not isinstance(props, dict) or not isinstance(models, dict) or not isinstance(models.get("data"), list):
            raise _Invalid("unknown upstream identity")
        build = props.get("build") or props.get("build_info")
        model = props.get("model_alias") or props.get("model")
        if requested_model is None:
            requested_model = model
        defaults = props.get("default_generation_settings")
        if not isinstance(defaults, dict):
            raise _Invalid("unknown upstream defaults")
        params = defaults.get("params", defaults)
        sampler_keys = ("temperature", "top_p", "top_k", "min_p", "repeat_last_n")
        complete_defaults = isinstance(params, dict) and all(isinstance(params.get(name, defaults.get(name)), _Number) for name in sampler_keys) and isinstance(params.get("repeat_penalty", params.get("repetition_penalty", defaults.get("repeat_penalty", defaults.get("repetition_penalty")))), _Number)
        selected = None
        for entry in models["data"]:
            if not isinstance(entry, dict) or type(entry.get("id")) is not str:
                continue
            aliases = entry.get("aliases", [])
            if not isinstance(aliases, list) or not all(type(alias) is str for alias in aliases):
                continue
            if requested_model in [entry["id"], *aliases] and model in [entry["id"], *aliases]:
                selected = entry
                break
        metadata = selected.get("meta", {}) if selected is not None else {}
        context = defaults.get("n_ctx", props.get("n_ctx", props.get("total_context", metadata.get("n_ctx") if isinstance(metadata, dict) else None)))
        if type(build) is not str or not build or type(model) is not str or not model or selected is None or not complete_defaults or type(props.get("chat_template")) is not str or not props.get("chat_template") or not isinstance(context, _Number) or not context.isascii() or not context.isdigit() or int(context) <= 0:
            raise _Invalid("incomplete or mismatched upstream identity")
        fields = ("version", "build", "build_info", "model", "model_alias", "model_path", "model_ftype", "chat_template", "chat_template_tool_use", "chat_template_caps", "n_ctx", "total_context", "n_ctx_per_seq", "n_ctx_slot", "n_parallel", "n_slots", "total_slots", "default_generation_settings", "modalities", "media_marker", "bos_token", "eos_token", "supports_streaming", "streaming", "supports_tokenization", "tokenization", "supports_structured_output", "structured_output", "grammar")
        stable_model = {key: selected[key] for key in ("id", "aliases", "architecture", "meta") if key in selected}
        # Observed metadata cannot detect an invisible in-place weights replacement: operator clear is required.
        return _canonical({"cache_schema": _SCHEMA, "prompt_schema": _PROMPT_SCHEMA, "upstream": self.upstream.geturl(), "props": {key: props[key] for key in fields if key in props}, "selected_model": stable_model})

    def retained(self, requested_model: object) -> tuple[int, str, int, str | None, bytes | None, str | None, int]:
        with self.planning_lock:
            if self.planning_revoked:
                raise _Invalid("retained planning identity was revoked")
            record = self.store.planning()
            generation = self.planning_generation
        value = _load(record[1].encode())
        if not isinstance(value, dict) or value.get("upstream") != self.upstream.geturl() or self.identity(value.get("props"), {"data": [value.get("selected_model")]}, requested_model) != record[1]:
            raise _Invalid("invalid retained planning identity")
        return (*record, generation)

    def admit_replay(self, record: tuple[int, str, int, str | None, bytes | None, str | None, int], requested_model: object) -> None:
        # Linearization point: validate current authority/epoch before admitting bytes. Socket writes stay outside.
        with self.planning_lock:
            if self.planning_revoked or record[6] != self.planning_generation:
                raise _Invalid("retained replay was revoked")
            current = self.retained(requested_model)
            if current[:2] != record[:2] or current[3:6] != record[3:6]:
                raise _Invalid("retained replay evidence changed")

    def read_identity(self, requested_model: object, authorization: str | None, budget: _Budget) -> tuple[str, tuple[int, str, int, str | None, bytes | None, str | None, int] | None]:
        if type(requested_model) is not str or not requested_model:
            raise _Invalid("cache lookup requires an explicit model alias")
        try:
            return self.observe(requested_model, authorization, budget), None
        except _Unavailable:
            record = self.retained(requested_model)
            return record[1], record


def _single_token(raw: bytes) -> bool:
    try:
        value = _load(raw)
        tokens = value.get("tokens") if isinstance(value, dict) else None
        return isinstance(tokens, list) and len(tokens) == 1 and isinstance(tokens[0], _Number) and tokens[0].isascii() and tokens[0].isdigit() and int(tokens[0]) <= 0xFFFFFFFF
    except (ValueError, TypeError, RecursionError):
        return False


def _read(response: http.client.HTTPResponse, budget: _Budget, limit: int) -> bytes:
    result = bytearray()
    while True:
        budget.remaining()
        chunk = response.read1(min(65536, limit + 1 - len(result)))
        if not chunk:
            if response.length not in {None, 0}:
                raise http.client.IncompleteRead(bytes(result), response.length)
            return bytes(result)
        result.extend(chunk)
        if len(result) > limit:
            raise _Invalid("response too large")


class _Handler(BaseHTTPRequestHandler):
    server: CacheServer
    protocol_version = "HTTP/1.1"

    def setup(self) -> None:
        self.budget = self.server.budgets[self.request]
        super().setup()
        self.started = False

    def log_message(self, *_args: object) -> None:
        pass

    def _headers(self, status: int, headers: list[tuple[str, str]], provenance: str, length: int | None) -> None:
        if not _valid_headers(headers):
            raise _Invalid("invalid upstream response headers")
        self.close_connection = True
        self.started = True
        self.send_response_only(status)
        connection_tokens = {token.strip().lower() for name, value in headers if name.lower() == "connection" for token in value.split(",")}
        for name, value in headers:
            if name.lower() not in _HOP_HEADERS | connection_tokens:
                self.send_header(name, value)
        self.send_header("X-Hymt-Cache", provenance)
        self.send_header("Connection", "close")
        if length is not None:
            self.send_header("Content-Length", str(length))
        self.end_headers()

    def _alive(self) -> None:
        if select.select([self.connection], [], [], 0)[0] and not self.connection.recv(1, socket.MSG_PEEK):
            raise ConnectionResetError("downstream cancelled")

    def _reply(self, status: int, provenance: str, raw: bytes = b'') -> None:
        self._headers(status, [("Content-Type", "application/json")], provenance, len(raw))
        self.wfile.write(raw)

    def do_GET(self) -> None:
        if self.path not in {"/props", "/v1/models"}:
            self._reply(404, "UNKNOWN")
            return
        live = None
        try:
            if self.headers.get("X-Hymt-Cache-Bypass") == "1" or self.headers.get("x-bf-eh-x-hymt-cache-bypass") == "1":
                # Transparent bypass does not interpret identity or update cache authority.
                with self.server.upstream_response("GET", self.path, None, self.headers.get("Authorization"), self.budget) as response:
                    raw = _read(response, self.budget, self.server.response_limit)
                    self._headers(response.status, response.getheaders(), "BYPASS", len(raw))
                    self.wfile.write(raw)
                return
            observation = self.server.begin_observation()
            probe = None
            try:
                probe = _Budget(min(2.0, self.budget.remaining() / 4), self.budget)
                try:
                    live = self.server.metadata(self.path, self.headers.get("Authorization"), probe)
                except _Unavailable:
                    pass
                if live is not None and live[0] not in {502, 503, 504}:
                    try:
                        self.server._observe(None, self.headers.get("Authorization"), probe, (self.path, live), observation=observation)
                    except (OSError, http.client.HTTPException, sqlite3.Error, ValueError, TypeError, RecursionError):
                        pass  # Optional historical proof must not replace a usable live response.
                    status, headers, raw = live
                    self._headers(status, headers, "BYPASS", len(raw))
                    self.wfile.write(raw)
                    return
                try:
                    self.server._observe(None, self.headers.get("Authorization"), probe, (self.path, live), observation=observation)
                except _Unavailable:
                    record = self.server.retained(None)
                else:
                    raise _Invalid("historical planning requires upstream unavailability")
            finally:
                if probe is not None:
                    probe.close()
                self.server.end_observation()
            value = _load(record[1].encode())
            body = value["props"] if self.path == "/props" else {"data": [value["selected_model"]]}
            headers = [("Content-Type", "application/json"), ("X-Hymt-Planning", "historical"), ("X-Hymt-Observed-At", str(record[2]))]
            raw = _canonical(body).encode()
            self.server.admit_replay(record, None)
            self._headers(200, headers, "HIT", len(raw))
            self.wfile.write(raw)
        except (OSError, http.client.HTTPException, sqlite3.Error, ValueError, TypeError, RecursionError):
            if not self.started:
                if live is not None and live[0] in {502, 503, 504}:
                    status, headers, raw = live
                    self._headers(status, headers, "BYPASS", len(raw))
                    self.wfile.write(raw)
                else:
                    self._reply(502, "UNKNOWN")

    def do_POST(self) -> None:
        if self.path not in {"/v1/chat/completions", "/tokenize"}:
            self._reply(404, "UNKNOWN")
            return
        flight = None
        leader = False
        key = ""
        hit = None
        provenance = "UNKNOWN"
        try:
            lengths = self.headers.get_all("Content-Length", [])
            if self.headers.get("Transfer-Encoding") or len(lengths) != 1 or not lengths[0].isascii() or not lengths[0].isdigit():
                self._reply(400, "UNKNOWN")
                return
            length = int(lengths[0])
            if length > self.server.request_limit:
                self._reply(413, "UNKNOWN")
                return
            raw = self.rfile.read(length)
            if len(raw) != length:
                self._reply(400, "UNKNOWN")
                return
            payload = _load(raw)
            if not isinstance(payload, dict):
                raise _Invalid("request must be a JSON object")
            if self.path == "/tokenize":
                authorization = self.headers.get("Authorization")
                bypass = self.headers.get("X-Hymt-Cache-Bypass") == "1" or self.headers.get("x-bf-eh-x-hymt-cache-bypass") == "1"
                canonical_token = _canonical(payload)
                identity = None
                token_candidate = set(payload) == {"content"} and type(payload["content"]) is str and len(raw) <= 4096
                if not bypass and token_candidate:
                    try:
                        identity = self.server.observe(None, authorization, self.budget)
                    except _Unavailable:
                        try:
                            record = self.server.retained(None)
                            body = record[4]
                            if record[3] == canonical_token and body is not None and hashlib.sha256(body).hexdigest() == record[5] and _single_token(body):
                                self.server.admit_replay(record, None)
                                self._reply(200, "HIT", body)
                                return
                        except (sqlite3.Error, ValueError, TypeError, RecursionError):
                            pass
                    except (OSError, http.client.HTTPException, sqlite3.Error, ValueError, TypeError, RecursionError):
                        pass
                with self.server.upstream_response("POST", self.path, raw, authorization, self.budget) as response:
                    body = _read(response, self.budget, self.server.response_limit)
                    headers = response.getheaders()
                    status = response.status
                if identity is not None and status == 200 and _cacheable_headers(headers) and _single_token(body):
                    try:
                        record = self.server.retained(None)
                        props = _load(record[1].encode())["props"]
                        # Retain only the advertised EOS mapping used by CLI planning, not arbitrary tokenization.
                        if payload == {"content": props.get("eos_token")} and record[1] == identity and self.server.observe(None, authorization, self.budget) == identity:
                            self.server.store.retain_token(record[0], identity, canonical_token, body)
                    except (OSError, http.client.HTTPException, sqlite3.Error, ValueError, TypeError, RecursionError):
                        pass
                self._headers(status, headers, "BYPASS", len(body))
                self.wfile.write(body)
                return
            canonical = _canonical(payload)
            authorization = self.headers.get("Authorization")
            bypass = self.headers.get("X-Hymt-Cache-Bypass") == "1" or self.headers.get("x-bf-eh-x-hymt-cache-bypass") == "1"
            identity = None
            epoch = None
            if bypass:
                provenance = "BYPASS"
            else:
                try:
                    identity, record = self.server.read_identity(payload.get("model"), authorization, self.budget)
                    fresh = record is None
                    if record is None:
                        record = self.server.retained(payload.get("model"))
                    if identity != record[1]:
                        raise _Invalid("planning identity changed")
                    epoch = record[0]
                    key = hashlib.sha256((identity + "\n" + str(epoch) + "\n" + canonical).encode()).hexdigest()
                    if not fresh:
                        hit = self.server.store.read(key, epoch, canonical, identity, self.server.request_limit, self.server.response_limit)
                        if hit is not None:
                            self.server.admit_replay(record, payload.get("model"))
                        identity = None
                    else:
                        with self.server.flight_lock:
                            flight = self.server.flights.get(key)
                            if flight is None:
                                flight = _Flight()
                                self.server.flights[key] = flight
                                leader = True
                        hit = self.server.store.read(key, epoch, canonical, identity, self.server.request_limit, self.server.response_limit) if leader else None
                        if hit is not None:
                            self.server.admit_replay(record, payload.get("model"))
                            flight.reply = (200, hit[0], hit[1])
                            flight.admitted = True
                        provenance = "MISS" if leader else "HIT"
                except (OSError, sqlite3.Error, ValueError, http.client.HTTPException, TypeError, RecursionError):
                    hit = None
                    identity = None
                    provenance = "UNKNOWN"
            if hit is not None:
                self._headers(200, hit[0], "HIT", len(hit[1]))
                self.wfile.write(hit[1])
                return
            if flight is not None and not leader:
                if not flight.done.wait(self.budget.remaining()) or flight.reply is None:
                    self._reply(502, "UNKNOWN")
                    return
                status, headers, body = flight.reply
                shared_status = "UNKNOWN"
                if flight.admitted:
                    try:
                        if self.server.store.epoch() == epoch and self.server.observe(payload.get("model"), authorization, self.budget) == identity:
                            self.server.admit_replay(record, payload.get("model"))
                            shared_status = "HIT"
                    except (OSError, sqlite3.Error, ValueError, http.client.HTTPException, TypeError, RecursionError):
                        pass
                self._headers(status, headers, shared_status, len(body))
                self.wfile.write(body)
                return
            with self.server.upstream_response("POST", self.path, raw, authorization, self.budget) as response:
                status, headers = response.status, response.getheaders()
                content_type = response.getheader("Content-Type", "")
                streamed = content_type.split(";", 1)[0].strip().lower() == "text/event-stream"
                if streamed:
                    self._headers(status, headers, provenance, None)
                    collected = bytearray()
                    while True:
                        self.budget.remaining()
                        chunk = response.read1(65536)
                        if not chunk:
                            if response.length not in {None, 0}:
                                raise http.client.IncompleteRead(bytes(collected), response.length)
                            break
                        if len(collected) + len(chunk) > self.server.response_limit:
                            raise _Invalid("response too large")
                        self._alive()
                        self.wfile.write(chunk)
                        self.wfile.flush()
                        collected.extend(chunk)
                    response_body = bytes(collected)
                else:
                    response_body = _read(response, self.budget, self.server.response_limit)
            if not streamed:
                self._alive()
            if identity is not None and epoch is not None and status == 200 and _complete(response_body, content_type):
                try:
                    if self.server.observe(payload.get("model"), authorization, self.budget) == identity:
                        with self.server.planning_lock:
                            self.server.admit_replay(record, payload.get("model"))
                            admitted = self.server.store.write(key, epoch, raw, canonical, identity, headers, response_body, self.server.request_limit, self.server.response_limit)
                        if flight is not None:
                            flight.admitted = admitted
                except (OSError, sqlite3.Error, ValueError, http.client.HTTPException, TypeError, RecursionError):
                    pass
            if not streamed:
                self._headers(status, headers, provenance, len(response_body))
                self.wfile.write(response_body)
                self.wfile.flush()
            if flight is not None:
                flight.reply = (status, headers, response_body)
        except (UnicodeError, json.JSONDecodeError, _Invalid, RecursionError) as error:
            if not self.started:
                self._reply(400 if isinstance(error, (UnicodeError, json.JSONDecodeError, RecursionError)) or str(error) in {"duplicate JSON key", "nonfinite JSON number", "request must be a JSON object"} else 502, provenance)
        except (OSError, http.client.HTTPException, sqlite3.Error):
            if not self.started:
                self._reply(502, "UNKNOWN")
        finally:
            if leader and flight is not None:
                with self.server.flight_lock:
                    self.server.flights.pop(key, None)
                    flight.done.set()


def main() -> None:
    """Serve the private gateway-facing cache, or atomically clear its generation."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("serve", "clear"))
    parser.add_argument("--database", type=Path, required=True)
    parser.add_argument("--upstream")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8402)
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--max-clients", type=int, default=16)
    parser.add_argument("--max-upstream", type=int, default=4)
    args = parser.parse_args()
    os.umask(0o077)
    if args.action == "clear":
        _Store(args.database).clear()
        return
    if not args.upstream:
        parser.error("serve requires --upstream")
    server = CacheServer((args.host, args.port), args.upstream, args.database, timeout=args.timeout, max_clients=args.max_clients, max_upstream=args.max_upstream)
    stop = threading.Event()

    def terminate(_signum: int, _frame: object) -> None:
        stop.set()

    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGINT, terminate)
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.1})
    thread.start()
    try:
        stop.wait()
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    main()
