"""Local inference cache and deterministic upstream for frontend parity tests."""
import importlib.util
import io
import json
import pathlib
import signal
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.dont_write_bytecode = True
script, root = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
spec = importlib.util.spec_from_file_location("hymt_cache", script)
cache_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache_module)
lock = threading.Lock()


def record(name, value):
    with lock, (root / name).open("a", encoding="utf-8") as output:
        output.write(json.dumps(value, ensure_ascii=False) + "\n")


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, value, content_type="application/json", status=200):
        raw = value if isinstance(value, bytes) else json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        if self.path == "/props":
            self.reply({"build_info": "fixture-b123", "version": "llama.cpp-6000",
                        "model_alias": "Hy-MT2-7B", "model_path": "/synthetic/model.gguf",
                        "chat_template": "<|eos|>", "eos_token": "$", "n_ctx": 24576, "n_ctx_per_seq": 8192,
                        "n_parallel": 3, "default_generation_settings": {"n_predict": 512,
                        "temperature": 0.7, "top_p": 0.6, "top_k": 20, "min_p": 0.0,
                        "repeat_penalty": 1.05, "repeat_last_n": 64}})
        elif self.path == "/v1/models":
            self.reply({"data": [{"id": "Hy-MT2-7B", "aliases": ["Hy-MT2-7B"],
                                  "meta": {"n_ctx": 8192, "n_params": 7000000000}}]})
        else:
            self.reply({}, status=404)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        if self.path == "/tokenize":
            self.reply({"tokens": [3]})
            return
        assert self.path == "/v1/chat/completions"
        payload = json.loads(raw)
        record("upstream.jsonl", payload)
        prompt = payload["messages"][-1]["content"]
        if "ERROROWNER" in prompt:
            self.reply({"error": "fixture rejection"}, status=401)
            return
        if "LONGOWNER" in prompt:
            text = "L" * 4100
        elif "FIRSTOWNER" in prompt:
            text = "FIRSTRESULT"
        elif "SECONDOWNER" in prompt:
            text = "SECONDRESULT"
        else:
            raise AssertionError(f"unexpected source in prompt: {prompt!r}")
        frames = [{"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": None}]},
                  {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}]
        data = "".join("data: " + json.dumps(frame) + "\n\n" for frame in frames)
        self.reply((data + "data: [DONE]\n\n").encode(), "text/event-stream")


class Observed(cache_module._Handler):
    def do_POST(self):
        if self.path == "/v1/chat/completions":
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            self.fixture_body = raw.decode("utf-8")
            self.rfile = io.BytesIO(raw)
        super().do_POST()

    def _headers(self, status, headers, provenance, length=None):
        if self.path == "/v1/chat/completions":
            record("requests.jsonl", {"body": self.fixture_body, "status": status,
                                      "cache": provenance})
        return super()._headers(status, headers, provenance, length)


upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
cache = cache_module.CacheServer(("127.0.0.1", 0), f"http://127.0.0.1:{upstream.server_port}",
                                 root / "cache.sqlite3", timeout=10)
cache.RequestHandlerClass = Observed
threads = [threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05})
           for server in (upstream, cache)]
for thread in threads:
    thread.start()
stop = threading.Event()
signal.signal(signal.SIGTERM, lambda *_args: stop.set())
(root / "port.tmp").write_text(str(cache.server_port))
(root / "port.tmp").replace(root / "port")
try:
    stop.wait()
finally:
    for server in (cache, upstream):
        server.shutdown()
        server.server_close()
    for thread in threads:
        thread.join()
