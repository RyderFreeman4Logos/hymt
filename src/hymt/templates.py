from __future__ import annotations

from enum import StrEnum
from typing import Iterable, Mapping


class TemplateType(StrEnum):
    DEFAULT = "default"
    TERMINOLOGY = "terminology"
    STYLE = "style"
    PERSONALIZATION = "personalization"
    DELIMITERS = "delimiters"
    STRUCTURED = "structured"
    CONTEXT_AWARE = "context"


LANGUAGE_NAMES: dict[str, str] = {
    "zh": "中文",
    "en": "English",
    "fr": "Français",
    "pt": "Português",
    "es": "Español",
    "ja": "日本語",
    "tr": "Türkçe",
    "ru": "Русский",
    "ar": "العربية",
    "ko": "한국어",
    "th": "ไทย",
    "it": "Italiano",
    "de": "Deutsch",
    "vi": "Tiếng Việt",
    "ms": "Bahasa Melayu",
    "id": "Bahasa Indonesia",
    "tl": "Tagalog",
    "hi": "हिन्दी",
    "pl": "Polski",
    "cs": "Čeština",
    "nl": "Nederlands",
    "km": "ខ្មែរ",
    "my": "မြန်မာ",
    "fa": "فارسی",
    "gu": "ગુજરાતી",
    "ur": "اردو",
    "te": "తెలుగు",
    "mr": "मराठी",
    "he": "עברית",
    "bn": "বাংলা",
    "ta": "தமிழ்",
    "uk": "Українська",
    "bo": "བོད་སྐད",
    "kk": "Қазақша",
    "mn": "Монгол",
    "ug": "ئۇيغۇرچە",
    "yue": "粤语",
}

SUPPORTED_LANGUAGES = frozenset(LANGUAGE_NAMES)
CHINESE_PROMPT_LANGUAGES = frozenset({"zh", "yue"})


def language_name(code: str) -> str:
    normalized = code.lower()
    try:
        return LANGUAGE_NAMES[normalized]
    except KeyError as exc:
        supported = ", ".join(sorted(SUPPORTED_LANGUAGES))
        raise ValueError(f"Unsupported target language '{code}'. Supported: {supported}") from exc


def build_prompt(
    source_text: str,
    target_lang: str,
    template_type: TemplateType | str = TemplateType.DEFAULT,
    **kwargs: object,
) -> str:
    selected_type = TemplateType(template_type)
    target_name = language_name(target_lang)
    chinese_prompt = target_lang.lower() in CHINESE_PROMPT_LANGUAGES
    match selected_type:
        case TemplateType.DEFAULT:
            return build_default_prompt(source_text, target_name, chinese_prompt)
        case TemplateType.TERMINOLOGY:
            return build_terminology_prompt(source_text, target_name, chinese_prompt, kwargs.get("terms"))
        case TemplateType.STYLE:
            return build_style_prompt(source_text, target_name, chinese_prompt, kwargs.get("style") or kwargs.get("target_style"))
        case TemplateType.PERSONALIZATION:
            return build_personalization_prompt(source_text, target_name, chinese_prompt, kwargs.get("instructions"))
        case TemplateType.DELIMITERS:
            return build_delimiters_prompt(source_text, target_name, chinese_prompt)
        case TemplateType.STRUCTURED:
            return build_structured_prompt(source_text, target_name, chinese_prompt, kwargs.get("format_type") or kwargs.get("format"))
        case TemplateType.CONTEXT_AWARE:
            return build_context_prompt(source_text, target_name, chinese_prompt, kwargs.get("background_text") or kwargs.get("context"))
        case _:
            raise ValueError(f"Unsupported template type '{selected_type}'")


def build_default_prompt(source_text: str, target_lang: str, chinese_prompt: bool = False) -> str:
    if chinese_prompt:
        return f"请将以下文本翻译成{target_lang}。注意，你应该只输出翻译结果，不要添加任何解释：\n\n{source_text}"
    return (
        f"Translate the following text into {target_lang}. "
        "Note that you should only output the translated result without any additional explanation:"
        f"\n\n{source_text}"
    )


def build_terminology_prompt(
    source_text: str,
    target_lang: str,
    chinese_prompt: bool = False,
    terms: object = None,
) -> str:
    normalized_terms = _normalize_terms(terms)
    if chinese_prompt:
        lines = [f"{source} 翻译为 {target}" for source, target in normalized_terms]
        reference = "\n".join(lines)
        return (
            f"请参考以下翻译：\n{reference}\n\n"
            f"请将以下文本翻译成{target_lang}。注意，你必须只输出翻译结果，不要添加任何解释："
            f"\n\n{source_text}"
        )
    lines = [f"{source} translates to {target}" for source, target in normalized_terms]
    reference = "\n".join(lines)
    return (
        f"Reference the following translations:\n{reference}\n\n"
        f"Translate the following text into {target_lang}. "
        "Note that you must ONLY output the translated result without any additional explanation:"
        f"\n\n{source_text}"
    )


def build_style_prompt(
    source_text: str,
    target_lang: str,
    chinese_prompt: bool = False,
    style: object = None,
) -> str:
    target_style = _require_text(style, "style")
    if chinese_prompt:
        return f"请将以下文本翻译成{target_lang}。注意，翻译风格必须严格符合[{target_style}]：\n\n{source_text}"
    return (
        f"Please translate the following text into {target_lang}. "
        f"Note that the translation style must strictly conform to [{target_style}]:"
        f"\n\n{source_text}"
    )


def build_personalization_prompt(
    source_text: str,
    target_lang: str,
    chinese_prompt: bool = False,
    instructions: object = None,
) -> str:
    normalized = _normalize_instructions(instructions)
    if chinese_prompt:
        tasks = [f"{index}. {instruction}" for index, instruction in enumerate(normalized, start=1)]
        tasks.append(f"{len(tasks) + 1}. 将[源文本]翻译成{target_lang}。")
        return f"[源文本]\n{source_text}\n\n[翻译任务]\n" + "\n".join(tasks)
    tasks = [f"{index}. {instruction}" for index, instruction in enumerate(normalized, start=1)]
    tasks.append(f"{len(tasks) + 1}. Translate the [Source Text] into {target_lang}.")
    return f"[Source Text]\n{source_text}\n\n[Translation Tasks]\n" + "\n".join(tasks)


def build_delimiters_prompt(source_text: str, target_lang: str, chinese_prompt: bool = False) -> str:
    if chinese_prompt:
        return (
            f"请准确地将以下文本翻译成{target_lang}。\n"
            "你必须在译文中保留完全相同数量的分隔符。严禁省略、转义或翻译这些符号，并请特别注意它们的位置。"
            f"\n\n{source_text}"
        )
    return (
        f"Please accurately translate the following text into {target_lang}.\n"
        "You must retain the exact same number of delimiters in the translation. "
        "Strictly do not omit, escape, or translate these symbols, and pay close attention to their placement."
        f"\n\n{source_text}"
    )


def build_structured_prompt(
    source_text: str,
    target_lang: str,
    chinese_prompt: bool = False,
    format_type: object = None,
) -> str:
    data_format = _require_text(format_type or "structured", "format_type")
    if chinese_prompt:
        return (
            "### 任务\n"
            f"将以下{data_format}数据中的用户可见文本翻译成{target_lang}。\n\n"
            "### 严格规则\n"
            "1. 结构保持：完全保留原始结构。\n"
            "2. 选择性翻译：只翻译可见的用户文本。\n"
            "3. 严格不翻译：绝不翻译代码标签、键、属性、占位符。\n\n"
            f"### 源数据\n{source_text}"
        )
    return (
        "### Task\n"
        f"Translate the user-facing text within the following {data_format} data into {target_lang}.\n\n"
        "### Strict Rules\n"
        "1. Structure Preservation: preserve original structure exactly.\n"
        "2. Selective Translation: translate ONLY visible user-facing text.\n"
        "3. Strict Non-Translation: NEVER translate code tags, keys, properties, placeholders.\n\n"
        f"### Source Data\n{source_text}"
    )


def build_context_prompt(
    source_text: str,
    target_lang: str,
    chinese_prompt: bool = False,
    background_text: object = None,
) -> str:
    background = _require_text(background_text, "context")
    if chinese_prompt:
        return (
            f"[背景信息]\n{background}\n\n"
            f"请结合所提供的背景信息，将以下文本翻译成{target_lang}。\n\n"
            f"[源文本]\n{source_text}"
        )
    return (
        f"[Background Information]\n{background}\n\n"
        f"Please translate the following text into {target_lang}, taking the provided background information into consideration.\n\n"
        f"[Source Text]\n{source_text}"
    )


def _normalize_terms(terms: object) -> list[tuple[str, str]]:
    if terms is None:
        return []
    if isinstance(terms, Mapping):
        return [(str(source), str(target)) for source, target in terms.items()]
    if not isinstance(terms, Iterable) or isinstance(terms, str):
        raise ValueError("terms must be an iterable of 'source=target' strings or pairs")
    normalized: list[tuple[str, str]] = []
    for term in terms:
        if isinstance(term, str):
            source, separator, target = term.partition("=")
            if not separator:
                raise ValueError(f"Invalid terminology pair '{term}'. Expected source=target")
            normalized.append((source, target))
            continue
        if isinstance(term, Iterable):
            pair = list(term)
            if len(pair) == 2:
                normalized.append((str(pair[0]), str(pair[1])))
                continue
        raise ValueError("terms must contain 'source=target' strings or two-item pairs")
    return normalized


def _normalize_instructions(instructions: object) -> list[str]:
    if instructions is None:
        return []
    if isinstance(instructions, str):
        return [instructions]
    if isinstance(instructions, Iterable):
        return [str(instruction) for instruction in instructions]
    raise ValueError("instructions must be a string or an iterable of strings")


def _require_text(value: object, name: str) -> str:
    if isinstance(value, str) and value:
        return value
    raise ValueError(f"{name} is required for this template type")
