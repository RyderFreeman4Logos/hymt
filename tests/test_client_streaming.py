from __future__ import annotations

import asyncio
import json
from types import SimpleNamespace
import unittest

import httpx

from hymt.client import TranslationClient


class ClientStreamingTests(unittest.TestCase):
    def test_translate_stream_yields_sse_tokens(self) -> None:
        seen_payload: dict[str, object] = {}

        def handler(request: httpx.Request) -> httpx.Response:
            seen_payload.update(json.loads(request.content.decode("utf-8")))
            return httpx.Response(
                200,
                headers={"content-type": "text/event-stream"},
                content=(
                    b'data: {"choices":[{"delta":{"content":"hello"}}]}\n\n'
                    b'data: {"choices":[{"delta":{"content":" world"}}]}\n\n'
                    b"data: [DONE]\n\n"
                ),
            )

        async def run() -> list[str]:
            async with httpx.AsyncClient(
                transport=httpx.MockTransport(handler)
            ) as http_client:
                client = TranslationClient(fake_config(), http_client)
                return [token async for token in client.translate_stream("prompt")]

        tokens = asyncio.run(run())

        self.assertEqual(tokens, ["hello", " world"])
        self.assertTrue(seen_payload["stream"])


def fake_config() -> SimpleNamespace:
    return SimpleNamespace(
        endpoint_url="http://127.0.0.1:8401/v1",
        api_key="",
        model="",
        timeout=30.0,
        concurrency=1,
        max_output_tokens=128,
        temperature=0.7,
        top_p=0.6,
        top_k=20,
        repetition_penalty=1.05,
        maybe_reload=lambda: False,
    )


if __name__ == "__main__":
    unittest.main()
