"""Causal cache contracts against a local fake llama.cpp server; stdlib only."""

import concurrent.futures
import hashlib
import http.client
import importlib.util
import json
import os
import socket
import sqlite3
import ssl
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

__all__ = ["CacheContracts"]

os.umask(0o077)
_SCRIPT = Path(__file__).resolve().parent.parent / "hymt-inference-cache.py"
_SPEC = importlib.util.spec_from_file_location("inference_cache", _SCRIPT)
_MODULE = None
if _SCRIPT.exists():
    assert _SPEC and _SPEC.loader
    _MODULE = importlib.util.module_from_spec(_SPEC)
    _SPEC.loader.exec_module(_MODULE)

_JSON = b'{ "model":"Hy-MT2-7B", "choices":[{"index":0,"message":{"content":" translation "},"finish_reason":"stop"}], "usage":{"prompt_tokens":17,"completion_tokens":3,"total_tokens":20},"system_fingerprint":"original" }'
_SSE_FIRST = b'data: {"choices":[{"index":0,"delta":{"content":" translation "},"finish_reason":null}]}\n\n'
_SSE_LAST = b'data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\ndata: {"choices":[],"usage":{"prompt_tokens":17,"completion_tokens":3,"total_tokens":20}}\n\ndata: [DONE]\n\n'


class _Upstream(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def handle(self) -> None:
        try:
            super().handle()
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, *_args: object) -> None:
        pass

    def do_GET(self) -> None:
        state = self.server.state
        if self.headers.get("Authorization") == "Bearer denied":
            self._reply(401, b'{"error":"denied"}')
            return
        if state["probe_stall"]:
            state["probe_release"].wait(2)
        status = state.get("metadata_status", {}).get(self.path)
        if status is not None:
            self._reply(status, b'{"error":"metadata rejected"}')
        elif state["unknown"] or state.get("offline"):
            self._reply(503, b'{"error":"unavailable"}')
        elif self.path == "/props":
            self._reply(200, state.get("props_raw") or json.dumps(state["props"]).encode())
        elif self.path == "/v1/models":
            self._reply(200, json.dumps({"data": [{"id": state.get("model_id", "Hy-MT2-7B"), "aliases": [state.get("model_id", "Hy-MT2-7B")], "created": state["generation"], "meta": state["meta"]}, *state["models_extra"]]}).encode())
        else:
            self._reply(404, b'{}')

    def _reply(self, status: int, raw: bytes, content_type: str = "application/json") -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("X-Original", "retained")
        for name, value in self.server.state.get("extra_headers", ()):
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self) -> None:
        state = self.server.state
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        payload = json.loads(raw)
        with state["lock"]:
            state["posts"] += 1
        state["entered"].set()
        state["release"].wait(2)
        if self.headers.get("Authorization") == "Bearer denied":
            self._reply(401, b'{"error":"denied"}')
            return
        if state.get("offline"):
            self._reply(503, b'{"error":"offline"}')
            return
        state.setdefault("requests", []).append((self.path, payload))
        if self.path == "/tokenize":
            self._reply(200, b'{"tokens":[42]}')
            return
        mode = payload.get("scenario", "ok")
        if mode == "error":
            self._reply(429, b'{"error":"original rate limit"}')
        elif mode == "length":
            self._reply(200, _JSON.replace(b'"stop"', b'"length"'))
        elif mode == "empty":
            self._reply(200, _JSON.replace(b' translation ', b''))
        elif mode == "bad":
            self._reply(200, b'not json')
        elif mode == "partial":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(_JSON) + 5))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(_JSON)
            self.close_connection = True
        elif mode == "large":
            self._reply(200, _JSON + b' ' * 4096)
        elif mode == "stall":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(_SSE_FIRST)
            self.wfile.flush()
            state["stream_release"].wait(2)
            self.close_connection = True
        elif payload.get("stream"):
            final = _SSE_LAST
            if mode == "sse_length":
                final = final.replace(b'"stop"', b'"length"')
            elif mode == "sse_partial":
                final = final.replace(b'data: [DONE]\n\n', b'')
            elif mode == "sse_bad":
                final = b'data: bad\n\ndata: [DONE]\n\n'
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream; charset=utf-8")
            self.send_header("Connection", "close")
            for name, value in state.get("extra_headers", ()):
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(_SSE_FIRST)
            self.wfile.flush()
            state["stream_first"].set()
            state["stream_release"].wait(2)
            try:
                self.wfile.write(final)
            except (BrokenPipeError, ConnectionResetError):
                pass
            self.close_connection = True
        else:
            self._reply(200, _JSON + b" " * state.get("padding", 0))


class CacheContracts(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="fixture-", dir=Path(os.environ["TMPDIR"]))
        self.db = Path(self.tmp.name) / "cache.sqlite3"
        self.upstream = ThreadingHTTPServer(("127.0.0.1", 0), _Upstream)
        self.upstream.daemon_threads = False
        self.state = {"lock": threading.Lock(), "posts": 0, "unknown": False, "generation": 1, "models_extra": [], "meta": {"n_ctx": 8192, "n_params": 7000000000}, "probe_stall": False, "probe_release": threading.Event(),
                      "entered": threading.Event(), "release": threading.Event(),
                      "stream_first": threading.Event(), "stream_release": threading.Event(),
                      "props": {"build_info": "b123", "version": "llama.cpp-6000", "model_alias": "Hy-MT2-7B",
                                "model_path": "/models/Hy-MT2-7B.gguf", "chat_template": "chatml",
                                "n_ctx": 24576, "n_ctx_per_seq": 8192, "n_parallel": 3,
                                "default_generation_settings": {"n_predict": 2048, "temperature": 0.7, "top_p": 0.6, "top_k": 20, "min_p": 0.0, "repeat_penalty": 1.05, "repeat_last_n": 64}}}
        self.state["release"].set()
        self.state["stream_release"].set()
        self.upstream.state = self.state
        self.up_thread = threading.Thread(target=self.upstream.serve_forever, kwargs={"poll_interval": 0.01})
        self.up_thread.start()
        self.cache = None
        self.cache_thread = None
        self.start_cache()

    def start_cache(self, upstream: str | None = None, **limits: object) -> None:
        if _MODULE is None:
            self.port = self.upstream.server_port
            return
        self.cache = _MODULE.CacheServer(("127.0.0.1", 0), upstream or f"http://127.0.0.1:{self.upstream.server_port}", self.db, **limits)
        self.port = self.cache.server_port
        self.cache_thread = threading.Thread(target=self.cache.serve_forever, kwargs={"poll_interval": 0.01})
        self.cache_thread.start()

    def stop_cache(self) -> None:
        if self.cache is not None:
            self.cache.shutdown()
            self.cache.server_close()
            self.cache_thread.join(3)
            self.assertFalse(self.cache_thread.is_alive())
            self.cache = None

    def tearDown(self) -> None:
        self.state["release"].set()
        self.state["stream_release"].set()
        self.state["probe_release"].set()
        self.stop_cache()
        self.upstream.shutdown()
        self.upstream.server_close()
        self.up_thread.join(3)
        self.assertFalse(self.up_thread.is_alive())
        self.tmp.cleanup()

    def request(self, payload: bytes | dict[str, object] | None = None, headers: dict[str, str] | None = None, path: str = "/v1/chat/completions") -> tuple[int, dict[str, str], bytes]:
        if payload is None:
            payload = {"model": "Hy-MT2-7B", "messages": [{"role": "user", "content": "same text"}]}
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=4)
        try:
            conn.request("POST", path, body=raw, headers={"Content-Type": "application/json", **(headers or {})})
            response = conn.getresponse()
            return response.status, dict(response.getheaders()), response.read()
        finally:
            conn.close()

    def raw_post(self, payload: dict[str, object]) -> bytes:
        raw = json.dumps(payload).encode()
        with socket.create_connection(("127.0.0.1", self.port), timeout=4) as conn:
            conn.settimeout(4)
            conn.sendall(b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: " + str(len(raw)).encode() + b"\r\nConnection: close\r\n\r\n" + raw)
            chunks = []
            while chunk := conn.recv(65536):
                chunks.append(chunk)
        return b"".join(chunks)

    def get_metadata(self, path="/props"):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=4)
        try:
            conn.request("GET", path)
            response = conn.getresponse()
            return response.status, dict(response.getheaders()), response.read()
        finally:
            conn.close()

    def test_offline_exact_hits_planning_and_restart(self):
        for stream in (False, True):
            payload = {"model": "Hy-MT2-7B", "stream": stream}
            self.assertEqual(self.request(payload)[1]["X-Hymt-Cache"], "MISS")
        self.state["props"].update(eos_token=".", supports_streaming=True, supports_tokenization=True, supports_structured_output=False)
        self.assertEqual(self.get_metadata()[0], 200)
        # Refresh both exact requests after the advertised capabilities change identity.
        for stream in (False, True):
            self.request({"model": "Hy-MT2-7B", "stream": stream})
        self.assertEqual(self.request({"content": "."}, path="/tokenize")[0], 200)
        self.assertEqual(self.request({"content": "unrelated"}, path="/tokenize")[0], 200)
        posts = self.state["posts"]
        self.state["unknown"] = True
        self.stop_cache()
        self.start_cache()
        status, headers, body = self.get_metadata()
        self.assertEqual(status, 200)
        self.assertEqual(headers.get("X-Hymt-Planning"), "historical")
        self.assertTrue(headers.get("X-Hymt-Observed-At", "").isdigit())
        self.assertTrue(json.loads(body)["supports_streaming"])
        self.assertEqual(self.request({"content": "."}, path="/tokenize")[1]["X-Hymt-Cache"], "HIT")
        for stream in (False, True):
            response = self.request({"model": "Hy-MT2-7B", "stream": stream})
            self.assertEqual(response[1]["X-Hymt-Cache"], "HIT")
        self.assertEqual(self.state["posts"], posts)
        self.cache.store.clear()
        self.assertNotEqual(self.get_metadata()[0], 200)
        self.assertNotEqual(self.request({"model": "Hy-MT2-7B", "stream": False})[1]["X-Hymt-Cache"], "HIT")

    def test_offline_does_not_resurrect_changed_identity(self):
        self.request()
        self.state["props"]["build_info"] = "new-build"
        self.assertEqual(self.get_metadata()[0], 200)
        self.state["unknown"] = True
        self.assertNotEqual(self.request()[1]["X-Hymt-Cache"], "HIT")

    def test_offline_miss_bypass_and_origin_change_require_backend(self):
        self.request()
        self.state["offline"] = True
        self.assertEqual(self.request()[1]["X-Hymt-Cache"], "HIT")
        for headers, payload in (({}, {"model": "Hy-MT2-7B", "different": True}), ({"X-Hymt-Cache-Bypass": "1"}, None), ({"x-bf-eh-x-hymt-cache-bypass": "1"}, None)):
            self.assertEqual(self.request(payload, headers)[0], 503)
        self.stop_cache()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            other_port = sock.getsockname()[1]
        self.start_cache(upstream=f"http://127.0.0.1:{other_port}")
        self.assertEqual(self.request()[0], 502)
        self.assertEqual(self.get_metadata()[0], 502)
        self.stop_cache()
        self.start_cache()
        self.assertEqual(self.get_metadata()[0], 502, "switching back must not resurrect old evidence")

    def test_stalled_upstream_probe_can_replay_within_request_budget(self):
        self.stop_cache()
        self.start_cache(timeout=0.6)
        self.request()
        self.state["probe_stall"] = True
        started = time.monotonic()
        self.assertEqual(self.get_metadata()[0], 200)
        self.assertEqual(self.request()[1]["X-Hymt-Cache"], "HIT")
        self.assertLess(time.monotonic() - started, 1.2)

    def test_missing_model_remains_ineligible_online_and_offline(self):
        for _ in range(2):
            self.assertEqual(self.request({})[1]["X-Hymt-Cache"], "UNKNOWN")
        self.assertEqual(self.state["posts"], 2)
        self.request()
        self.state["offline"] = True
        self.assertEqual(self.request({})[0], 503)

    def test_active_model_replacement_revokes_old_requested_alias(self):
        self.request()
        self.state["model_id"] = "replacement"
        self.state["props"]["model_alias"] = "replacement"
        self.assertNotEqual(self.request()[1]["X-Hymt-Cache"], "HIT")
        self.state["unknown"] = True
        self.assertNotEqual(self.request()[1]["X-Hymt-Cache"], "HIT")

    def test_denied_malformed_tls_and_local_capacity_do_not_fallback(self):
        self.request()
        for status in (401, 403):
            self.state["metadata_status"] = {"/props": status}
            self.assertNotEqual(self.request()[1]["X-Hymt-Cache"], "HIT")
            self.assertEqual(self.get_metadata()[0], 502)
        self.state.pop("metadata_status")
        for raw in (b'not-json', b'{"build":1,"build":2}', b'{}'):
            self.state["props_raw"] = raw
            self.assertNotEqual(self.request()[1]["X-Hymt-Cache"], "HIT")
            self.assertEqual(self.get_metadata()[0], 502)
        self.state.pop("props_raw")
        self.state["unknown"] = True
        for headers in ([("Content-Encoding", "gzip")], [("Content-Type", "application/json")]):
            self.state["extra_headers"] = headers
            self.assertNotEqual(self.request()[1]["X-Hymt-Cache"], "HIT")
            self.assertEqual(self.get_metadata()[0], 502)
        self.state.pop("extra_headers")
        self.state["unknown"] = False
        with patch.object(http.client.HTTPConnection, "getresponse", side_effect=http.client.BadStatusLine("synthetic malformed status")):
            budget = _MODULE._Budget(1)
            try:
                with self.assertRaises(http.client.BadStatusLine):
                    self.cache.read_identity("Hy-MT2-7B", None, budget)
            finally:
                budget.close()
        with patch.object(socket.socket, "connect", side_effect=ssl.SSLCertVerificationError("synthetic verification failure")):
            budget = _MODULE._Budget(1)
            try:
                with self.assertRaises(ssl.SSLCertVerificationError):
                    self.cache.read_identity("Hy-MT2-7B", None, budget)
            finally:
                budget.close()
        with patch.object(self.cache.controls, "acquire", return_value=False):
            self.assertNotEqual(self.request()[1]["X-Hymt-Cache"], "HIT")
            self.assertEqual(self.get_metadata()[0], 502)

    def test_partial_metadata_conflict_revokes_retained_evidence(self):
        for unavailable_path in ("/props", "/v1/models"):
            with self.subTest(unavailable=unavailable_path):
                self.state["unknown"] = False
                self.state.pop("metadata_status", None)
                self.request()
                if unavailable_path == "/v1/models":
                    self.state["props"]["build_info"] += "changed"
                else:
                    self.state["meta"]["n_params"] += 1
                self.state["metadata_status"] = {unavailable_path: 503}
                self.assertNotEqual(self.request()[1]["X-Hymt-Cache"], "HIT")
                self.state.pop("metadata_status")
                self.state["unknown"] = True
                self.assertEqual(self.get_metadata()[0], 502)
                self.assertNotEqual(self.request()[1]["X-Hymt-Cache"], "HIT")

    def test_corrupt_oversized_and_legacy_planning_are_ineligible(self):
        self.request()
        with sqlite3.connect(self.db) as conn:
            original = conn.execute("SELECT * FROM planning").fetchone()
        for assignment, value in (("identity", "{}"), ("identity", "x" * (1024 * 1024 + 1)), ("epoch", 9999), ("observed", -1)):
            with self.subTest(column=assignment, size=len(str(value))):
                with sqlite3.connect(self.db) as conn:
                    conn.execute("DELETE FROM planning")
                    conn.execute("INSERT INTO planning VALUES (?,?,?,?,?,?,?)", original)
                    conn.execute(f"UPDATE planning SET {assignment}=?", (value,))
                self.state["unknown"] = True
                self.assertEqual(self.get_metadata()[0], 502)
                self.assertNotEqual(self.request()[1]["X-Hymt-Cache"], "HIT")
        with sqlite3.connect(self.db) as conn:
            conn.execute("DELETE FROM planning")
        self.assertEqual(self.get_metadata()[0], 502)

    def test_failed_post_probe_cannot_admit_under_retained_identity(self):
        self.state["release"].clear()
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            job = pool.submit(self.request)
            self.assertTrue(self.state["entered"].wait(1))
            self.state["unknown"] = True
            self.state["release"].set()
            self.assertEqual(job.result(4)[0], 200)
        self.state["unknown"] = False
        self.assertEqual(self.request()[1]["X-Hymt-Cache"], "MISS")
        self.assertEqual(self.state["posts"], 2)

    def test_timeout_limits_reject_before_database_creation(self) -> None:
        upstream = f"http://127.0.0.1:{self.upstream.server_port}"
        for index, timeout in enumerate((float("nan"), float("inf"), float("-inf"), 1e100, 0, -1)):
            database = Path(self.tmp.name) / f"invalid-timeout-{index}.sqlite3"
            with self.subTest(timeout=timeout):
                try:
                    server = _MODULE.CacheServer(("127.0.0.1", 0), upstream, database, timeout=timeout)
                except ValueError:
                    pass
                else:
                    server.server_close()
                    self.fail("invalid timeout was accepted")
                self.assertFalse(database.exists())
        database = Path(self.tmp.name) / "valid-timeout.sqlite3"
        for timeout in (0.1, threading.TIMEOUT_MAX):
            with self.subTest(timeout=timeout):
                server = _MODULE.CacheServer(("127.0.0.1", 0), upstream, database, timeout=timeout)
                server.server_close()

    def test_two_credentials_share_exact_bytes(self) -> None:
        first = self.request(headers={"Authorization": "Bearer first"})
        second = self.request(headers={"Authorization": "Bearer second"})
        self.assertEqual(1, self.state["posts"], "equal payloads across credentials must execute upstream once")
        self.assertEqual("MISS", first[1].get("X-Hymt-Cache"))
        self.assertEqual("HIT", second[1].get("X-Hymt-Cache"))
        self.assertEqual(_JSON, first[2])
        self.assertEqual(first[2], second[2])
        self.assertEqual("retained", second[1].get("X-Original"))
        denied = self.request(headers={"Authorization": "Bearer denied"})
        self.assertEqual(401, denied[0])
        self.assertEqual("UNKNOWN", denied[1].get("X-Hymt-Cache"))

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_concurrent_duplicates_singleflight(self) -> None:
        self.state["release"].clear()
        with concurrent.futures.ThreadPoolExecutor(max_workers=5) as pool:
            jobs = [pool.submit(self.request) for _ in range(5)]
            self.assertTrue(self.state["entered"].wait(1))
            self.state["release"].set()
            results = [job.result(4) for job in jobs]
        self.assertEqual(1, self.state["posts"])
        self.assertEqual([_JSON] * 5, [result[2] for result in results])
        self.assertEqual(1, sum(result[1]["X-Hymt-Cache"] == "MISS" for result in results))

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_exact_request_and_observed_identity(self) -> None:
        base = b'{"model":"Hy-MT2-7B","messages":[" Ab "],"v":1}'
        self.request(base)
        self.assertEqual("HIT", self.request(b'{"v":1,"messages":[" Ab "],"model":"Hy-MT2-7B"}')[1]["X-Hymt-Cache"])
        for raw in (base.replace(b' Ab ', b'ab'), base.replace(b' Ab ', b' Ab'), base.replace(b'1}', b'1.0}'), base.replace(b'1}', b'true}'), base.replace(b'[" Ab "]', b'[" Ab ","next"]')):
            self.assertEqual("MISS", self.request(raw)[1]["X-Hymt-Cache"])
        for key, value in (("build_info", "b124"), ("chat_template", "other"), ("n_ctx", 16384), ("model_path", "/models/other.gguf")):
            self.state["props"][key] = value
            self.assertEqual("MISS", self.request(base)[1]["X-Hymt-Cache"])
        self.state["props"]["default_generation_settings"]["temperature"] = 0.8
        self.assertEqual("MISS", self.request(base)[1]["X-Hymt-Cache"])
        self.state["generation"] += 1
        self.assertEqual("HIT", self.request(base)[1]["X-Hymt-Cache"])
        self.assertEqual("UNKNOWN", self.request(base.replace(b'Hy-MT2-7B', b'other-model'))[1]["X-Hymt-Cache"])
        for raw in (b'{"model":"a","model":"b"}', b'{"v":NaN}', b'{"v":Infinity}', b'[]'):
            self.assertEqual(400, self.request(raw)[0])

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_explicit_bypass_clear_and_inflight_invalidation(self) -> None:
        self.request()
        for name in ("X-Hymt-Cache-Bypass", "x-bf-eh-x-hymt-cache-bypass"):
            self.assertEqual("BYPASS", self.request(headers={name: "1"})[1]["X-Hymt-Cache"])
        self.assertEqual(3, self.state["posts"])
        self.assertEqual("HIT", self.request(headers={"Cache-Control": "no-cache"})[1]["X-Hymt-Cache"])
        self.cache.store.clear()
        self.assertEqual("MISS", self.request()[1]["X-Hymt-Cache"])
        self.state["release"].clear()
        self.state["entered"].clear()
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            pending = pool.submit(self.request, {"model": "Hy-MT2-7B", "messages": ["inflight"]})
            self.assertTrue(self.state["entered"].wait(1))
            self.cache.store.clear()
            self.state["release"].set()
            pending.result(4)
        self.assertEqual("MISS", self.request({"model": "Hy-MT2-7B", "messages": ["inflight"]})[1]["X-Hymt-Cache"])

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_explicit_bypass_neither_reads_nor_writes_cache(self) -> None:
        for name in ("X-Hymt-Cache-Bypass", "x-bf-eh-x-hymt-cache-bypass"):
            with self.subTest(header=name):
                payload = {"model": "Hy-MT2-7B", "messages": [{"role": "user", "content": name}]}
                before = self.state["posts"]
                self.assertEqual("BYPASS", self.request(payload, {name: "1"})[1]["X-Hymt-Cache"])
                self.assertEqual(before + 1, self.state["posts"])
                miss = self.request(payload)
                self.assertEqual("MISS", miss[1]["X-Hymt-Cache"])
                self.assertEqual("HIT", self.request(payload)[1]["X-Hymt-Cache"])
                self.assertEqual(before + 2, self.state["posts"])
                self.state["padding"] = 1
                bypass = self.request(payload, {name: "1"})
                self.assertEqual("BYPASS", bypass[1]["X-Hymt-Cache"])
                self.assertNotEqual(miss[2], bypass[2])
                hit = self.request(payload)
                self.assertEqual("HIT", hit[1]["X-Hymt-Cache"])
                self.assertEqual(miss[2], hit[2])
                self.assertEqual(before + 3, self.state["posts"])
                self.state["padding"] = 0

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_stream_miss_is_live_and_hit_replays_exact_usage(self) -> None:
        self.state["stream_release"].clear()
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=2)
        try:
            conn.request("POST", "/v1/chat/completions", json.dumps({"model": "Hy-MT2-7B", "stream": True}))
            response = conn.getresponse()
            self.assertEqual("MISS", response.getheader("X-Hymt-Cache"))
            first = response.read(len(_SSE_FIRST))
            self.assertEqual(_SSE_FIRST, first, "first token must arrive before upstream releases its finish frame")
            self.assertFalse(self.state["stream_release"].is_set())
            self.state["stream_release"].set()
            raw = first + response.read()
        finally:
            self.state["stream_release"].set()
            conn.close()
        hit = self.request({"model": "Hy-MT2-7B", "stream": True})
        self.assertEqual("HIT", hit[1]["X-Hymt-Cache"])
        self.assertEqual(_SSE_FIRST + _SSE_LAST, raw)
        self.assertEqual(raw, hit[2])
        self.assertEqual(1, self.state["posts"])

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_bad_and_partial_responses_never_admit(self) -> None:
        for mode in ("error", "length", "empty", "bad", "sse_length", "sse_partial", "sse_bad", "partial"):
            with self.subTest(mode=mode):
                payload = {"model": "Hy-MT2-7B", "scenario": mode, "stream": mode.startswith("sse")}
                for _ in range(2):
                    try:
                        reply = self.request(payload)
                        if mode == "error":
                            self.assertEqual((429, b'{"error":"original rate limit"}'), (reply[0], reply[2]))
                        self.assertNotEqual("HIT", reply[1]["X-Hymt-Cache"])
                    except http.client.IncompleteRead:
                        self.assertEqual("partial", mode)
        self.assertEqual(16, self.state["posts"])
        self.state["unknown"] = True
        self.assertEqual("UNKNOWN", self.request()[1]["X-Hymt-Cache"])
        self.state["unknown"] = False
        self.assertEqual("MISS", self.request()[1]["X-Hymt-Cache"])

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_observable_drift_cannot_admit(self) -> None:
        self.state["release"].clear()
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            job = pool.submit(self.request)
            self.assertTrue(self.state["entered"].wait(1))
            self.state["props"]["build_info"] = "replacement"
            self.state["release"].set()
            job.result(4)
        self.assertEqual("MISS", self.request()[1]["X-Hymt-Cache"])
        self.assertEqual(2, self.state["posts"])

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_restart_corruption_and_optional_database_failure(self) -> None:
        self.request()
        self.stop_cache()
        self.start_cache()
        self.assertEqual("HIT", self.request()[1]["X-Hymt-Cache"])
        with sqlite3.connect(self.db) as conn:
            conn.execute("UPDATE entries SET response = ?", (b'{"choices":[]}',))
        self.assertEqual("MISS", self.request()[1]["X-Hymt-Cache"])
        with sqlite3.connect(self.db) as conn:
            conn.execute("DROP TABLE entries")
        self.assertEqual("UNKNOWN", self.request()[1]["X-Hymt-Cache"])
        self.assertEqual(0o600, self.db.stat().st_mode & 0o777)

    def test_reopened_limit_and_poisoned_rows_regenerate(self) -> None:
        self.state["padding"] = 4096
        self.assertEqual("MISS", self.request()[1]["X-Hymt-Cache"])
        self.stop_cache()
        self.start_cache(response_limit=1024)
        self.state["padding"] = 0
        reply = self.request()
        self.assertEqual((200, "MISS", _JSON), (reply[0], reply[1]["X-Hymt-Cache"], reply[2]))
        self.stop_cache()
        self.start_cache()
        good_headers = json.dumps([["Content-Type", "application/json"], ["X-Original", "retained"]])
        for column, value in (("request", b"x" * (2 * 1024 * 1024 + 1)),
                              ("request", "not a blob"),
                              ("response", _JSON.decode()),
                              ("canonical", "x" * (2 * 1024 * 1024 + 1)),
                              ("identity", "x" * (2 * 1024 * 1024 + 1)),
                              ("digest", b"x" * 64),
                              ("headers", good_headers.encode()),
                              ("headers", json.dumps([["Content-Type", "application/json"], ["X-Pad", "x" * (1024 * 1024)]])),
                              ("headers", json.dumps([["Content-Type", "application/json"]] + [["X-Extra", "v"]] * 100))):
            with self.subTest(column=column, size=len(value)):
                with sqlite3.connect(self.db) as conn:
                    conn.execute(f"UPDATE entries SET {column}=?", (value,))
                raw = self.raw_post({"model": "Hy-MT2-7B", "messages": [{"role": "user", "content": "same text"}]})
                self.assertEqual(1, raw.count(b"HTTP/1.1 "))
                self.assertIn(b"X-Hymt-Cache: MISS\r\n", raw)
                self.assertEqual(_JSON, raw.split(b"\r\n\r\n", 1)[1])

        too_large = _JSON + b" " * (8 * 1024 * 1024)
        with sqlite3.connect(self.db) as conn:
            conn.execute("UPDATE entries SET response=?, digest=?", (too_large, hashlib.sha256(too_large).hexdigest()))
        reply = self.request()
        self.assertEqual((200, "MISS", _JSON), (reply[0], reply[1]["X-Hymt-Cache"], reply[2]))

    def test_poisoned_stored_headers_never_start_a_bad_response(self) -> None:
        payload = {"model": "Hy-MT2-7B", "messages": [{"role": "user", "content": "same text"}]}
        for headers in ([['Content-Type', 'application/json'], ['X-Note', '\u0100']],
                        [['Content-Type', 'application/json'], ['Bad(Name', 'value']],
                        [['Content-Type', 'application/json'], ['X-Note', 'bad\x00value']],
                        [['Content-Type', 'application/json'], ['X-Note', 'bad\x7fvalue']],
                        [['Content-Type', 'application/json'], ['Content-Type', 'text/event-stream']],
                        [['Connection', 'Content-Type'], ['Content-Type', 'application/json']],
                        [['Content-Type', 'application/json'], ['Content-Encoding', 'gzip']]):
            with self.subTest(headers=headers):
                self.request(payload)
                with sqlite3.connect(self.db) as conn:
                    conn.execute("UPDATE entries SET headers=?", (json.dumps(headers),))
                before = self.state["posts"]
                raw = self.raw_post(payload)
                self.assertEqual(1, raw.count(b"HTTP/1.1 "))
                self.assertTrue(raw.startswith(b"HTTP/1.1 200 "), raw[:200])
                self.assertIn(b"X-Hymt-Cache: MISS\r\n", raw)
                self.assertEqual(_JSON, raw.split(b"\r\n\r\n", 1)[1])
                self.assertEqual(before + 1, self.state["posts"])

        streaming = {**payload, "stream": True}
        self.request(streaming)
        with sqlite3.connect(self.db) as conn:
            conn.execute("UPDATE entries SET headers=? WHERE response=?", (json.dumps([["Content-Type", "text/event-stream"], ["X-Note", "bad\x00value"]]), _SSE_FIRST + _SSE_LAST))
        before = self.state["posts"]
        raw = self.raw_post(streaming)
        self.assertEqual(1, raw.count(b"HTTP/1.1 "))
        self.assertIn(b"X-Hymt-Cache: MISS\r\n", raw)
        self.assertEqual(_SSE_FIRST + _SSE_LAST, raw.split(b"\r\n\r\n", 1)[1])
        self.assertEqual(before + 1, self.state["posts"])

    def test_bad_upstream_fields_fail_before_http_delivery(self) -> None:
        try:
            for extra in (("X-Note", "bad\x00value"), ("X-Note", "bad\x7fvalue"), ("Bad(Name", "value")):
                with self.subTest(extra=extra):
                    self.state["extra_headers"] = [extra]
                    for payload in ({"model": "Hy-MT2-7B"}, {"model": "Hy-MT2-7B", "stream": True}):
                        raw = self.raw_post(payload)
                        self.assertEqual(1, raw.count(b"HTTP/1.1 "))
                        self.assertTrue(raw.startswith(b"HTTP/1.1 502 "), raw[:200])
                    self.assertEqual(502, self.request({"content": "."}, path="/tokenize")[0])
                    conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=4)
                    try:
                        conn.request("GET", "/props")
                        self.assertEqual(502, conn.getresponse().status)
                    finally:
                        conn.close()
        finally:
            self.state.pop("extra_headers", None)

    def test_valid_wire_fields_and_large_metadata_admission(self) -> None:
        self.state["extra_headers"] = [("X-Empty", ""), ("X-Note!", "caf\xe9\tok"), ("X-Repeat", "a"), ("X-Repeat", "b"), ("Connection", "X-Hop"), ("X-Hop", "omit")]
        try:
            self.assertEqual("MISS", self.request()[1]["X-Hymt-Cache"])
            raw = self.raw_post({"model": "Hy-MT2-7B", "messages": [{"role": "user", "content": "same text"}]})
            self.assertIn(b"X-Hymt-Cache: HIT\r\n", raw)
            self.assertIn(b"X-Empty: \r\n", raw)
            self.assertIn(b"X-Note!: caf\xe9\tok\r\n", raw)
            self.assertEqual(2, raw.count(b"X-Repeat:"))
            self.assertNotIn(b"X-Hop:", raw)
            self.assertEqual(1, self.state["posts"])
            self.cache.store.clear()
            self.state["extra_headers"] = [(f"X-Large-{i}", "x" * 60000) for i in range(18)]
            for _ in range(2):
                raw = self.raw_post({"model": "Hy-MT2-7B"})
                self.assertIn(b"X-Hymt-Cache: MISS\r\n", raw)
                self.assertEqual(_JSON, raw.split(b"\r\n\r\n", 1)[1])
            self.assertEqual(3, self.state["posts"])
        finally:
            self.state.pop("extra_headers", None)

    def test_slow_optional_read_does_not_block_unrelated_flight(self) -> None:
        first = {"model": "Hy-MT2-7B", "messages": ["first"]}
        second = {"model": "Hy-MT2-7B", "messages": ["second"]}
        entered = threading.Event()
        release = threading.Event()
        original = self.cache.store.read
        def paused_read(*args, **kwargs):
            if not entered.is_set():
                entered.set()
                release.wait(2)
            return original(*args, **kwargs)
        try:
            with patch.object(self.cache.store, "read", paused_read), concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
                blocked = pool.submit(self.request, first)
                self.assertTrue(entered.wait(1))
                same_key = pool.submit(self.request, first)
                unrelated = pool.submit(self.request, second)
                self.assertTrue(self.state["entered"].wait(1), "unrelated key must reach upstream during optional read")
                release.set()
                first_reply = blocked.result(4)
                shared_reply = same_key.result(4)
                self.assertEqual(("MISS", _JSON), (first_reply[1]["X-Hymt-Cache"], first_reply[2]))
                self.assertEqual(("HIT", _JSON), (shared_reply[1]["X-Hymt-Cache"], shared_reply[2]))
                self.assertEqual(_JSON, unrelated.result(4)[2])
        finally:
            release.set()
        self.assertEqual(2, self.state["posts"])

    def test_disconnected_cache_hit_never_regenerates(self) -> None:
        self.stop_cache()
        self.start_cache(timeout=3)
        self.state["padding"] = 6 * 1024 * 1024
        self.assertEqual("MISS", self.request()[1]["X-Hymt-Cache"])
        self.state["padding"] = 0
        raw = json.dumps({"model": "Hy-MT2-7B", "messages": [{"role": "user", "content": "same text"}]}).encode()
        conn = socket.create_connection(("127.0.0.1", self.port), timeout=3)
        try:
            conn.settimeout(3)
            conn.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1024)
            conn.sendall(b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: " + str(len(raw)).encode() + b"\r\n\r\n" + raw)
            headers = b""
            while b"\r\n\r\n" not in headers and len(headers) < 65536:
                headers += conn.recv(4096)
            self.assertIn(b"X-Hymt-Cache: HIT", headers.split(b"\r\n\r\n", 1)[0])
            conn.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        finally:
            conn.close()
        self.stop_cache()
        self.assertEqual(1, self.state["posts"], "a broken HIT must not start another upstream request")

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_recursive_optional_identity_and_stored_headers_preserve_generation(self) -> None:
        deep = b"[" * 1100 + b"0" + b"]" * 1100
        props = json.dumps(self.state["props"]).encode()
        for raw in (deep, props[:-1] + b',"chat_template_caps":' + b"[" * 400 + b"0" + b"]" * 400 + b"}"):
            with self.subTest(identity=raw[:20]):
                self.state["props_raw"] = raw
                reply = self.request()
                self.assertEqual((200, "UNKNOWN", _JSON), (reply[0], reply[1]["X-Hymt-Cache"], reply[2]))
        self.state["props_raw"] = None
        self.assertEqual("MISS", self.request()[1]["X-Hymt-Cache"])
        with sqlite3.connect(self.db) as conn:
            conn.execute("UPDATE entries SET headers = ?", (deep.decode(),))
        reply = self.request()
        self.assertEqual((200, "UNKNOWN", _JSON), (reply[0], reply[1]["X-Hymt-Cache"], reply[2]))
        self.assertEqual(4, self.state["posts"])
        client_deep = b'{"model":"Hy-MT2-7B","nested":' + deep + b"}"
        self.assertEqual(400, self.request(client_deep)[0])
        self.assertEqual(4, self.state["posts"], "invalid client JSON must not reach generation")

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_recursive_post_generation_identity_preserves_json_and_sse_flights(self) -> None:
        deep = b"[" * 1100 + b"0" + b"]" * 1100
        joined = threading.Event()
        original = _MODULE._Flight
        class ObservedEvent(threading.Event):
            def wait(inner, timeout=None):
                joined.set()
                return super().wait(timeout)
        class ObservedFlight(original):
            def __init__(inner):
                super().__init__()
                inner.done = ObservedEvent()
        for stream, expected in ((False, _JSON), (True, _SSE_FIRST + _SSE_LAST)):
            with self.subTest(stream=stream):
                self.cache.store.clear()
                self.state["props_raw"] = None
                self.state["entered"].clear()
                self.state["release"].clear()
                joined.clear()
                payload = {"model": "Hy-MT2-7B", "stream": stream}
                try:
                    with patch.object(_MODULE, "_Flight", ObservedFlight), concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                        leader = pool.submit(self.request, payload)
                        self.assertTrue(self.state["entered"].wait(1))
                        follower = pool.submit(self.request, payload)
                        self.assertTrue(joined.wait(1), "follower must join the active flight")
                        self.state["props_raw"] = deep
                        self.state["release"].set()
                        first, second = leader.result(4), follower.result(4)
                    self.assertEqual((200, expected), (first[0], first[2]))
                    self.assertEqual((200, "UNKNOWN", expected), (second[0], second[1]["X-Hymt-Cache"], second[2]))
                    with sqlite3.connect(self.db) as conn:
                        self.assertEqual(0, conn.execute("SELECT count(*) FROM entries").fetchone()[0])
                finally:
                    self.state["release"].set()

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_recursive_follower_revalidation_preserves_completed_reply(self) -> None:
        deep = b"[" * 1100 + b"0" + b"]" * 1100
        joined = threading.Event()
        resume = threading.Event()
        original = _MODULE._Flight
        class PausedEvent(threading.Event):
            def wait(inner, timeout=None):
                joined.set()
                done = super().wait(timeout)
                resume.wait(2)
                return done
        class PausedFlight(original):
            def __init__(inner):
                super().__init__()
                inner.done = PausedEvent()
        self.state["entered"].clear()
        self.state["release"].clear()
        try:
            with patch.object(_MODULE, "_Flight", PausedFlight), concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                leader = pool.submit(self.request)
                self.assertTrue(self.state["entered"].wait(1))
                follower = pool.submit(self.request)
                self.assertTrue(joined.wait(1), "follower must await the completed leader")
                self.state["release"].set()
                first = leader.result(4)
                self.assertEqual((200, "MISS", _JSON), (first[0], first[1]["X-Hymt-Cache"], first[2]))
                self.state["props_raw"] = deep
                resume.set()
                second = follower.result(4)
            self.assertEqual((200, "UNKNOWN", _JSON), (second[0], second[1]["X-Hymt-Cache"], second[2]))
            self.assertEqual(1, self.state["posts"])
        finally:
            self.state["release"].set()
            resume.set()

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_body_response_deadline_and_follower_limits(self) -> None:
        self.stop_cache()
        self.start_cache(timeout=0.4, request_limit=512, response_limit=512, max_clients=3, max_upstream=1)
        self.assertEqual(413, self.request(b' ' * 513)[0])
        for _ in range(2):
            self.request({"model": "Hy-MT2-7B", "scenario": "large"})
        self.assertEqual(2, self.state["posts"])
        self.state["stream_release"].clear()
        started = time.monotonic()
        try:
            self.request({"model": "Hy-MT2-7B", "scenario": "stall", "stream": True})
        except (OSError, http.client.HTTPException):
            pass
        self.assertLess(time.monotonic() - started, 1.2)
        retained = self.cache
        self.stop_cache()
        self.assertEqual({}, retained.flights)
        self.assertEqual({}, retained.budgets)
        self.state["stream_release"].set()

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_cancelled_request_and_atomic_cli_clear(self) -> None:
        self.state["release"].clear()
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=2)
        conn.request("POST", "/v1/chat/completions", json.dumps({"model": "Hy-MT2-7B"}))
        self.assertTrue(self.state["entered"].wait(1))
        conn.sock.shutdown(socket.SHUT_RDWR)
        conn.close()
        self.state["release"].set()
        self.assertEqual("MISS", self.request({"model": "Hy-MT2-7B"})[1]["X-Hymt-Cache"])
        cleared = subprocess.run([sys.executable, "-B", str(_SCRIPT), "clear", "--database", str(self.db)], capture_output=True, timeout=3, check=False)
        self.assertEqual(0, cleared.returncode, cleared.stderr)
        self.assertEqual("MISS", self.request({"model": "Hy-MT2-7B"})[1]["X-Hymt-Cache"])

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_incomplete_defaults_and_partial_transaction_are_safe(self) -> None:
        self.request()
        self.assertEqual("HIT", self.request()[1]["X-Hymt-Cache"])
        defaults = self.state["props"].pop("default_generation_settings")
        self.assertEqual("UNKNOWN", self.request()[1]["X-Hymt-Cache"])
        self.state["props"]["default_generation_settings"] = defaults
        del defaults["min_p"]
        self.assertEqual("UNKNOWN", self.request()[1]["X-Hymt-Cache"])
        defaults["min_p"] = 0.0
        conn = sqlite3.connect(self.db)
        conn.execute("BEGIN IMMEDIATE")
        conn.execute("DELETE FROM entries")
        conn.close()
        self.stop_cache()
        self.start_cache()
        self.assertEqual("HIT", self.request()[1]["X-Hymt-Cache"])

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_client_capacity_rejects_without_unbounded_threads(self) -> None:
        self.stop_cache()
        self.start_cache(timeout=1, max_clients=2, max_upstream=1)
        idle = [socket.create_connection(("127.0.0.1", self.port), timeout=2) for _ in range(2)]
        try:
            with socket.create_connection(("127.0.0.1", self.port), timeout=2) as overloaded:
                self.assertTrue(overloaded.recv(512).startswith(b"HTTP/1.1 503 Service Unavailable"))
        finally:
            for conn in idle:
                conn.close()
        retained = self.cache
        started = time.monotonic()
        self.stop_cache()
        self.assertLess(time.monotonic() - started, 0.5)
        self.assertEqual({}, retained.budgets)

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_real_nested_props_and_volatile_model_metadata(self) -> None:
        props = self.state["props"]
        params = props["default_generation_settings"]
        props["default_generation_settings"] = {"n_ctx": 8192, "params": params}
        props["total_slots"] = props.pop("n_parallel")
        del props["n_ctx"]
        del props["n_ctx_per_seq"]
        self.assertEqual("MISS", self.request()[1]["X-Hymt-Cache"])
        self.assertEqual("HIT", self.request()[1]["X-Hymt-Cache"])
        self.state["generation"] += 1
        self.state["models_extra"] = [{"id": "unrelated", "created": 123, "meta": {"n_ctx": 1024}}]
        self.assertEqual("HIT", self.request()[1]["X-Hymt-Cache"])
        self.state["meta"]["n_params"] += 1
        self.assertEqual("MISS", self.request()[1]["X-Hymt-Cache"])
        props["total_slots"] += 1
        self.assertEqual("MISS", self.request()[1]["X-Hymt-Cache"])

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_numeric_content_and_unfinished_choices_never_admit(self) -> None:
        for content_type, raw in [
            ("application/json", b'{"choices":[{"index":0,"finish_reason":"stop","message":{"content":123}}]}'),
            ("text/event-stream", b'data: {"choices":[{"index":0,"delta":{"content":123},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'),
            ("text/event-stream", b'data: {"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"},{"index":1,"delta":{"role":"assistant"},"finish_reason":null}]}\n\ndata: [DONE]\n\n'),
            ("text/event-stream", b'data: {"choices":[{"index":-1,"delta":{"content":"ok"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'),
            ("text/event-stream", b'data: {"choices":[{"index":0.0,"delta":{"content":"ok"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n')]:
            with self.subTest(raw=raw):
                self.assertFalse(_MODULE._complete(raw, content_type))

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_waiting_followers_require_actual_current_admission(self) -> None:
        self.stop_cache()
        self.start_cache(max_upstream=1)
        original = _MODULE._Flight
        for mode in ("drift", "clear", "outage", "write_failure"):
            self.cache.store.clear()
            self.state["entered"].clear()
            self.state["release"].clear()
            joined = threading.Event()
            class ObservedEvent(threading.Event):
                def wait(inner, timeout=None, barrier=joined):
                    barrier.set()
                    return super().wait(timeout)
            class ObservedFlight(original):
                def __init__(inner):
                    super().__init__()
                    inner.done = ObservedEvent()
            with patch.object(_MODULE, "_Flight", ObservedFlight), concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                leader = pool.submit(self.request)
                self.assertTrue(self.state["entered"].wait(1))
                follower = pool.submit(self.request)
                self.assertTrue(joined.wait(1), "real follower must reach flight wait")
                if mode == "drift":
                    self.state["props"]["build_info"] += "replacement"
                elif mode == "clear":
                    self.cache.store.clear()
                elif mode == "outage":
                    self.state["unknown"] = True
                else:
                    with sqlite3.connect(self.db) as conn:
                        conn.execute("DROP TABLE entries")
                self.state["release"].set()
                first, second = leader.result(3), follower.result(3)
            self.assertEqual(_JSON, first[2])
            self.assertEqual(_JSON, second[2])
            self.assertEqual("UNKNOWN", second[1]["X-Hymt-Cache"], mode)
            self.state["unknown"] = False

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_stalled_optional_probe_preserves_actual_generation(self) -> None:
        self.stop_cache()
        self.start_cache(timeout=0.6, max_upstream=1)
        self.state["probe_stall"] = True
        started = time.monotonic()
        reply = self.request()
        self.assertEqual((200, "UNKNOWN", _JSON), (reply[0], reply[1]["X-Hymt-Cache"], reply[2]))
        self.assertEqual(1, self.state["posts"])
        self.assertLess(time.monotonic() - started, 0.6)
        self.state["probe_release"].set()

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_tcp_and_tls_share_absolute_deadline(self) -> None:
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        release = threading.Event()
        def accept():
            conn, _ = listener.accept()
            try:
                release.wait(2)
            finally:
                conn.close()
        thread = threading.Thread(target=accept)
        thread.start()
        port = listener.getsockname()[1]
        server = _MODULE.CacheServer(("127.0.0.1", 0), f"https://127.0.0.1:{port}", Path(self.tmp.name)/"tls.sqlite3", timeout=0.5)
        budget = _MODULE._Budget(0.5)
        original = socket.socket.connect
        phase = threading.Event()
        def delayed_connect(sock, address):
            if address[1] == port:
                phase.wait(0.3)
            return original(sock, address)
        started = time.monotonic()
        try:
            with patch.object(socket.socket, "connect", delayed_connect), self.assertRaises(OSError), server.upstream_response("GET", "/props", None, None, budget):
                self.fail("stalled TLS unexpectedly completed")
            elapsed = time.monotonic() - started
            self.assertLess(elapsed, 0.65)
            print(json.dumps({"case": "TCP_TLS_absolute_deadline", "budget_seconds": 0.5, "TCP_phase_delay": 0.3, "elapsed_seconds": elapsed}))
        finally:
            budget.close()
            server.server_close()
            release.set()
            listener.close()
            thread.join(2)
            self.assertFalse(thread.is_alive())

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_false_eos_tokenization_is_uncached_passthrough(self) -> None:
        for _ in range(2):
            reply = self.request({"content": ".", "add_special": False}, path="/tokenize")
            self.assertEqual((200, "BYPASS", b'{"tokens":[42]}'), (reply[0], reply[1]["X-Hymt-Cache"], reply[2]))
        self.assertEqual(2, self.state["posts"])

    @unittest.skipUnless(_MODULE, "shared service not implemented yet")
    def test_passthrough_preflight_and_no_admin_http(self) -> None:
        for path in ("/props", "/v1/models", "/admin/clear"):
            conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=2)
            try:
                conn.request("GET", path)
                response = conn.getresponse()
                raw = response.read()
                if path == "/admin/clear":
                    self.assertEqual(404, response.status)
                else:
                    self.assertEqual(200, response.status)
                    self.assertIsInstance(json.loads(raw), dict)
            finally:
                conn.close()


if __name__ == "__main__":
    program = unittest.main(verbosity=2, exit=False)
    remaining = [thread.name for thread in threading.enumerate() if thread is not threading.main_thread()]
    assert not remaining, remaining
    print(json.dumps({"runner_pid": os.getpid(), "owned_runtime_resources": 0, "tests": program.result.testsRun, "remaining_threads": remaining}))
    sys.exit(not program.result.wasSuccessful())
