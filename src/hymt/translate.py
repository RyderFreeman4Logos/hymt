from __future__ import annotations

from pathlib import Path
import sys

from hymt.client import TranslationClient
from hymt.config import HotConfig
from hymt.segment import Segmenter, ensure_tokenizer
from hymt.templates import TemplateType, build_prompt


async def translate_text(
    text: str,
    target_lang: str,
    config: HotConfig,
    template_type: TemplateType = TemplateType.DEFAULT,
    **template_kwargs: object,
) -> str:
    if not text:
        return ""

    tokenizer_path = ensure_tokenizer()
    segmenter = Segmenter(tokenizer_path)
    overhead_prompt = build_prompt("", target_lang, template_type, **template_kwargs)
    prompt_overhead_tokens = segmenter.count_tokens(overhead_prompt)
    available_source_tokens = config.context_window - prompt_overhead_tokens - config.max_output_tokens
    if available_source_tokens <= 0:
        raise ValueError(
            "Config context_window is too small for the selected template and max_output_tokens"
        )

    source_tokens = segmenter.count_tokens(text)
    segments = segmenter.segment(text, available_source_tokens)
    print(f"Source tokens: {source_tokens}; segments: {len(segments)}", file=sys.stderr)
    prompts = [build_prompt(segment, target_lang, template_type, **template_kwargs) for segment in segments]

    def report_progress(done: int, total: int) -> None:
        if total > 1:
            print(f"[{done}/{total}] Translating segment...", file=sys.stderr)

    async with TranslationClient(config) as client:
        translations = await client.translate_batch(prompts, on_progress=report_progress)
    return "".join(translations)


async def translate_file(
    input_path: Path,
    output_path: Path | None,
    target_lang: str,
    config: HotConfig,
    template_type: TemplateType = TemplateType.DEFAULT,
    **template_kwargs: object,
) -> None:
    text = input_path.read_text(encoding="utf-8")
    translated = await translate_text(text, target_lang, config, template_type, **template_kwargs)
    if output_path is None:
        sys.stdout.write(translated)
        return
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(translated, encoding="utf-8")
