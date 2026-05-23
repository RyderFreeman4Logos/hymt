from __future__ import annotations

from collections.abc import Callable
import asyncio
import json

import httpx

from hymt.config import HotConfig


RETRYABLE_STATUS_CODES = frozenset({429, 500, 502, 503})


class TranslationError(RuntimeError):
    pass


class TranslationClient:
    def __init__(
        self, config: HotConfig, http_client: httpx.AsyncClient | None = None
    ) -> None:
        self._config = config
        self._client = http_client or httpx.AsyncClient(
            timeout=httpx.Timeout(config.timeout)
        )
        self._owns_client = http_client is None
        self._concurrency = config.concurrency
        self._semaphore = asyncio.Semaphore(self._concurrency)
        self._semaphore_lock = asyncio.Lock()

    async def __aenter__(self) -> TranslationClient:
        return self

    async def __aexit__(self, exc_type: object, exc: object, traceback: object) -> None:
        await self.close()

    async def close(self) -> None:
        if self._owns_client:
            await self._client.aclose()

    async def translate(self, prompt: str) -> str:
        self._config.maybe_reload()
        payload = self._build_payload(prompt)
        headers = self._build_headers()
        url = self._chat_url()
        response_data = await self._post_with_retry(url, payload, headers)
        return _extract_translation(response_data)

    async def translate_batch(
        self,
        prompts: list[str],
        on_progress: Callable[[int, int], None] | None = None,
    ) -> list[str]:
        total = len(prompts)
        completed = 0
        progress_lock = asyncio.Lock()

        async def run(prompt: str) -> str:
            nonlocal completed
            self._config.maybe_reload()
            semaphore = await self._current_semaphore()
            async with semaphore:
                result = await self.translate(prompt)
            if on_progress is not None:
                async with progress_lock:
                    completed += 1
                    on_progress(completed, total)
            return result

        return await asyncio.gather(*(run(prompt) for prompt in prompts))

    async def _current_semaphore(self) -> asyncio.Semaphore:
        concurrency = self._config.concurrency
        if concurrency == self._concurrency:
            return self._semaphore
        async with self._semaphore_lock:
            concurrency = self._config.concurrency
            if concurrency != self._concurrency:
                self._concurrency = concurrency
                self._semaphore = asyncio.Semaphore(concurrency)
            return self._semaphore

    def _build_payload(self, prompt: str) -> dict[str, object]:
        payload: dict[str, object] = {
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": self._config.max_output_tokens,
            "temperature": self._config.temperature,
            "top_p": self._config.top_p,
            "top_k": self._config.top_k,
            "repetition_penalty": self._config.repetition_penalty,
        }
        if self._config.model:
            payload["model"] = self._config.model
        return payload

    def _build_headers(self) -> dict[str, str]:
        headers = {"Content-Type": "application/json"}
        if self._config.api_key:
            headers["Authorization"] = f"Bearer {self._config.api_key}"
        return headers

    def _chat_url(self) -> str:
        return f"{self._config.endpoint_url}/chat/completions"

    async def _post_with_retry(
        self,
        url: str,
        payload: dict[str, object],
        headers: dict[str, str],
        max_retries: int = 5,
    ) -> dict[str, object]:
        last_error: Exception | None = None
        for attempt in range(max_retries + 1):
            try:
                response = await self._client.post(url, json=payload, headers=headers)
                if response.status_code < 400:
                    return _decode_json_response(response)
                error = TranslationError(_format_response_error(response))
                if not _is_retryable_response(response) or attempt == max_retries:
                    raise error
                last_error = error
            except (
                httpx.TimeoutException,
                httpx.TransportError,
                json.JSONDecodeError,
            ) as exc:
                if attempt == max_retries:
                    raise TranslationError("Translation request failed") from exc
                last_error = exc
            await asyncio.sleep(min(0.5 * (2**attempt), 8.0))
        raise TranslationError("Translation request failed") from last_error


def _decode_json_response(response: httpx.Response) -> dict[str, object]:
    data = response.json()
    if not isinstance(data, dict):
        raise TranslationError("Translation response must be a JSON object")
    return data


def _is_retryable_response(response: httpx.Response) -> bool:
    if response.status_code in RETRYABLE_STATUS_CODES:
        return True
    if response.status_code != 400:
        return False
    body = response.text.lower()
    return "json" in body or "parse" in body


def _format_response_error(response: httpx.Response) -> str:
    return f"HTTP {response.status_code}: {response.text[:500]}"


def _extract_translation(data: dict[str, object]) -> str:
    choices = data.get("choices")
    if not isinstance(choices, list) or not choices:
        raise TranslationError("Translation response missing choices")
    first = choices[0]
    if not isinstance(first, dict):
        raise TranslationError("Translation response choice must be an object")
    message = first.get("message")
    if isinstance(message, dict):
        content = message.get("content")
        if isinstance(content, str):
            return content
    text = first.get("text")
    if isinstance(text, str):
        return text
    raise TranslationError("Translation response missing message content")
