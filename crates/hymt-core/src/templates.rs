//! Prompt template builder — seven template types ported from `templates.py`.

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

// ── Language registry ────────────────────────────────────────────────────────

/// Returns the display name for a BCP-47-style language code.
pub fn language_name(code: &str) -> Result<&'static str, CoreError> {
    match code.to_lowercase().as_str() {
        "zh" => Ok("中文"),
        "en" => Ok("English"),
        "fr" => Ok("Français"),
        "pt" => Ok("Português"),
        "es" => Ok("Español"),
        "ja" => Ok("日本語"),
        "tr" => Ok("Türkçe"),
        "ru" => Ok("Русский"),
        "ar" => Ok("العربية"),
        "ko" => Ok("한국어"),
        "th" => Ok("ไทย"),
        "it" => Ok("Italiano"),
        "de" => Ok("Deutsch"),
        "vi" => Ok("Tiếng Việt"),
        "ms" => Ok("Bahasa Melayu"),
        "id" => Ok("Bahasa Indonesia"),
        "tl" => Ok("Tagalog"),
        "hi" => Ok("हिन्दी"),
        "pl" => Ok("Polski"),
        "cs" => Ok("Čeština"),
        "nl" => Ok("Nederlands"),
        "km" => Ok("ខ្មែរ"),
        "my" => Ok("မြန်မာ"),
        "fa" => Ok("فارسی"),
        "gu" => Ok("ગુજરાતી"),
        "ur" => Ok("اردو"),
        "te" => Ok("తెలుగు"),
        "mr" => Ok("मराठी"),
        "he" => Ok("עברית"),
        "bn" => Ok("বাংলা"),
        "ta" => Ok("தமிழ்"),
        "uk" => Ok("Українська"),
        "bo" => Ok("བོད་སྐད"),
        "kk" => Ok("Қазақша"),
        "mn" => Ok("Монгол"),
        "ug" => Ok("ئۇيغۇرچە"),
        "yue" => Ok("粤语"),
        other => Err(CoreError::UnsupportedLanguage(other.to_owned())),
    }
}

fn is_chinese_prompt_lang(code: &str) -> bool {
    matches!(code.to_lowercase().as_str(), "zh" | "yue")
}

// ── Template type ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TemplateType {
    Default,
    Terminology,
    Style,
    Personalization,
    Delimiters,
    Structured,
    #[serde(rename = "context")]
    ContextAware,
}

impl TemplateType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Terminology => "terminology",
            Self::Style => "style",
            Self::Personalization => "personalization",
            Self::Delimiters => "delimiters",
            Self::Structured => "structured",
            Self::ContextAware => "context",
        }
    }
}

impl TryFrom<&str> for TemplateType {
    type Error = CoreError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "default" => Ok(Self::Default),
            "terminology" => Ok(Self::Terminology),
            "style" => Ok(Self::Style),
            "personalization" => Ok(Self::Personalization),
            "delimiters" => Ok(Self::Delimiters),
            "structured" => Ok(Self::Structured),
            "context" => Ok(Self::ContextAware),
            other => Err(CoreError::InvalidTemplate(other.to_owned())),
        }
    }
}

impl std::fmt::Display for TemplateType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Options ──────────────────────────────────────────────────────────────────

/// Extra parameters for prompt templates that require them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PromptOpts {
    /// Terminology pairs for `Terminology` template: `(source_term, target_term)`.
    pub terms: Option<Vec<(String, String)>>,
    /// Translation style description for `Style` template.
    pub style: Option<String>,
    /// Additional translation instructions for `Personalization` template.
    pub instructions: Option<Vec<String>>,
    /// Data format label for `Structured` template (e.g. `"JSON"`, `"YAML"`).
    pub format_type: Option<String>,
    /// Background/context paragraph for `ContextAware` template.
    pub context: Option<String>,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Builds a prompt string for the given `template`, `source` text, and `target_lang`.
///
/// `target_lang` must be a recognized language code (see `language_name`).
/// Some templates require additional opts; returns `Err` when they are absent.
pub fn build_prompt(
    source: &str,
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
) -> Result<String, CoreError> {
    let target_name = language_name(target_lang)?;
    let chinese = is_chinese_prompt_lang(target_lang);

    let prompt = match template {
        TemplateType::Default => build_default(source, target_name, chinese),
        TemplateType::Terminology => {
            let terms = opts.terms.as_deref().unwrap_or(&[]);
            build_terminology(source, target_name, chinese, terms)
        }
        TemplateType::Style => {
            let style = opts
                .style
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| CoreError::MissingTemplateOption("style".to_owned()))?;
            build_style(source, target_name, chinese, style)
        }
        TemplateType::Personalization => {
            let instructions = opts.instructions.as_deref().unwrap_or(&[]);
            build_personalization(source, target_name, chinese, instructions)
        }
        TemplateType::Delimiters => build_delimiters(source, target_name, chinese),
        TemplateType::Structured => {
            let fmt = opts
                .format_type
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("structured");
            build_structured(source, target_name, chinese, fmt)
        }
        TemplateType::ContextAware => {
            let ctx = opts
                .context
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| CoreError::MissingTemplateOption("context".to_owned()))?;
            build_context(source, target_name, chinese, ctx)
        }
    };
    Ok(prompt)
}

// ── Per-template builders ─────────────────────────────────────────────────────

fn build_default(source: &str, target_name: &str, chinese: bool) -> String {
    let cli_help_note = cli_help_prompt_note(source, chinese);
    if chinese {
        format!(
            "请将以下文本翻译成{target_name}。注意，你应该只输出翻译结果，不要添加任何解释：{cli_help_note}\n\n{source}"
        )
    } else {
        format!(
            "Translate the following text into {target_name}. \
Note that you should only output the translated result without any additional explanation:\
{cli_help_note}\n\n{source}"
        )
    }
}

fn cli_help_prompt_note(source: &str, chinese: bool) -> &'static str {
    if !looks_like_cli_help_source(source) {
        return "";
    }
    if chinese {
        " 如果源文本是命令行帮助、用法或选项列表，它本身就是完整待译内容；请逐项翻译 Usage/Arguments/Options/Examples 等标题和说明，保留命令、参数占位符以及 -/-- 选项，不要要求用户再提供输入。"
    } else {
        " If the source is command-line help, usage, or an option list, that help text is the complete source to translate; translate Usage/Arguments/Options/Examples headings and descriptions item by item, preserve commands, placeholders, and -/-- options, and do not ask the user to provide more input."
    }
}

fn looks_like_cli_help_source(source: &str) -> bool {
    let lower = source.to_lowercase();
    lower.contains("usage:")
        && (lower.contains("options:")
            || lower.contains("arguments:")
            || lower.contains("commands:")
            || lower.contains("examples:"))
        && lower.contains("--")
}

fn build_terminology(
    source: &str,
    target_name: &str,
    chinese: bool,
    terms: &[(String, String)],
) -> String {
    if chinese {
        let ref_lines: Vec<String> = terms
            .iter()
            .map(|(s, t)| format!("{s} 翻译为 {t}"))
            .collect();
        let reference = ref_lines.join("\n");
        format!(
            "请参考以下翻译：\n{reference}\n\n\
请将以下文本翻译成{target_name}。注意，你必须只输出翻译结果，不要添加任何解释：\n\n{source}"
        )
    } else {
        let ref_lines: Vec<String> = terms
            .iter()
            .map(|(s, t)| format!("{s} translates to {t}"))
            .collect();
        let reference = ref_lines.join("\n");
        format!(
            "Reference the following translations:\n{reference}\n\n\
Translate the following text into {target_name}. \
Note that you must ONLY output the translated result without any additional explanation:\
\n\n{source}"
        )
    }
}

fn build_style(source: &str, target_name: &str, chinese: bool, style: &str) -> String {
    if chinese {
        format!(
            "请将以下文本翻译成{target_name}。注意，翻译风格必须严格符合[{style}]：\n\n{source}"
        )
    } else {
        format!(
            "Please translate the following text into {target_name}. \
Note that the translation style must strictly conform to [{style}]:\
\n\n{source}"
        )
    }
}

fn build_personalization(
    source: &str,
    target_name: &str,
    chinese: bool,
    instructions: &[String],
) -> String {
    if chinese {
        let mut tasks: Vec<String> = instructions
            .iter()
            .enumerate()
            .map(|(i, inst)| format!("{}. {inst}", i + 1))
            .collect();
        tasks.push(format!(
            "{}. 将[源文本]翻译成{target_name}。",
            tasks.len() + 1
        ));
        format!("[源文本]\n{source}\n\n[翻译任务]\n{}", tasks.join("\n"))
    } else {
        let mut tasks: Vec<String> = instructions
            .iter()
            .enumerate()
            .map(|(i, inst)| format!("{}. {inst}", i + 1))
            .collect();
        tasks.push(format!(
            "{}. Translate the [Source Text] into {target_name}.",
            tasks.len() + 1
        ));
        format!(
            "[Source Text]\n{source}\n\n[Translation Tasks]\n{}",
            tasks.join("\n")
        )
    }
}

fn build_delimiters(source: &str, target_name: &str, chinese: bool) -> String {
    if chinese {
        format!(
            "请准确地将以下文本翻译成{target_name}。\n\
你必须在译文中保留完全相同数量的分隔符。严禁省略、转义或翻译这些符号，并请特别注意它们的位置。\
\n\n{source}"
        )
    } else {
        format!(
            "Please accurately translate the following text into {target_name}.\n\
You must retain the exact same number of delimiters in the translation. \
Strictly do not omit, escape, or translate these symbols, and pay close attention to their placement.\
\n\n{source}"
        )
    }
}

fn build_structured(source: &str, target_name: &str, chinese: bool, fmt: &str) -> String {
    if chinese {
        format!(
            "### 任务\n\
将以下{fmt}数据中的用户可见文本翻译成{target_name}。\n\n\
### 严格规则\n\
1. 结构保持：完全保留原始结构。\n\
2. 选择性翻译：只翻译可见的用户文本。\n\
3. 严格不翻译：绝不翻译代码标签、键、属性、占位符。\n\n\
### 源数据\n{source}"
        )
    } else {
        format!(
            "### Task\n\
Translate the user-facing text within the following {fmt} data into {target_name}.\n\n\
### Strict Rules\n\
1. Structure Preservation: preserve original structure exactly.\n\
2. Selective Translation: translate ONLY visible user-facing text.\n\
3. Strict Non-Translation: NEVER translate code tags, keys, properties, placeholders.\n\n\
### Source Data\n{source}"
        )
    }
}

fn build_context(source: &str, target_name: &str, chinese: bool, background: &str) -> String {
    if chinese {
        format!(
            "[背景信息]\n{background}\n\n\
请结合所提供的背景信息，将以下文本翻译成{target_name}。\n\n\
[源文本]\n{source}"
        )
    } else {
        format!(
            "[Background Information]\n{background}\n\n\
Please translate the following text into {target_name}, \
taking the provided background information into consideration.\n\n\
[Source Text]\n{source}"
        )
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> PromptOpts {
        PromptOpts::default()
    }

    #[test]
    fn language_name_known() {
        assert_eq!(language_name("zh").unwrap(), "中文");
        assert_eq!(language_name("en").unwrap(), "English");
        assert_eq!(language_name("ZH").unwrap(), "中文"); // case-insensitive
    }

    #[test]
    fn language_name_unknown() {
        assert!(matches!(
            language_name("xx"),
            Err(CoreError::UnsupportedLanguage(_))
        ));
    }

    #[test]
    fn template_type_round_trip() {
        for (s, expected) in &[
            ("default", TemplateType::Default),
            ("terminology", TemplateType::Terminology),
            ("style", TemplateType::Style),
            ("personalization", TemplateType::Personalization),
            ("delimiters", TemplateType::Delimiters),
            ("structured", TemplateType::Structured),
            ("context", TemplateType::ContextAware),
        ] {
            let t = TemplateType::try_from(*s).unwrap();
            assert_eq!(&t, expected);
            assert_eq!(t.as_str(), *s);
        }
    }

    #[test]
    fn template_type_invalid() {
        assert!(matches!(
            TemplateType::try_from("bogus"),
            Err(CoreError::InvalidTemplate(_))
        ));
    }

    // ── Default template ─────────────────────────────────────────────────────

    #[test]
    fn default_prompt_to_english() {
        let p = build_prompt("你好", "en", &TemplateType::Default, &opts()).unwrap();
        assert!(p.contains("Translate the following text into English"));
        assert!(p.contains("你好"));
        assert!(p.contains("without any additional explanation"));
    }

    #[test]
    fn default_prompt_to_chinese() {
        let p = build_prompt("Hello", "zh", &TemplateType::Default, &opts()).unwrap();
        assert!(p.contains("翻译成中文"));
        assert!(p.contains("Hello"));
        assert!(p.contains("只输出翻译结果"));
        assert!(!p.contains("命令行帮助"));
    }

    #[test]
    fn default_prompt_for_cli_help_treats_help_as_complete_source() {
        let source = "Usage: verbatim ask [OPTIONS] <QUESTION>...\n\n\
Options:\n  --source-id <SOURCE_ID>\n  --context-only\n\n\
Examples:\n  verbatim ask \"What supports this?\"\n";
        let p = build_prompt(source, "zh", &TemplateType::Default, &opts()).unwrap();

        assert!(p.contains("命令行帮助"));
        assert!(p.contains("它本身就是完整待译内容"));
        assert!(p.contains("不要要求用户再提供输入"));
        assert!(p.contains("--source-id"));
    }

    #[test]
    fn default_prompt_to_yue() {
        let p = build_prompt("Hello", "yue", &TemplateType::Default, &opts()).unwrap();
        assert!(p.contains("翻译成粤语"));
    }

    // ── Terminology template ─────────────────────────────────────────────────

    #[test]
    fn terminology_prompt_english_terms() {
        let o = PromptOpts {
            terms: Some(vec![
                ("API".to_owned(), "接口".to_owned()),
                ("cache".to_owned(), "缓存".to_owned()),
            ]),
            ..Default::default()
        };
        let p = build_prompt("Use the API cache.", "zh", &TemplateType::Terminology, &o).unwrap();
        assert!(p.contains("API 翻译为 接口"));
        assert!(p.contains("cache 翻译为 缓存"));
        assert!(p.contains("Use the API cache."));
    }

    #[test]
    fn terminology_prompt_no_terms() {
        let p = build_prompt("text", "en", &TemplateType::Terminology, &opts()).unwrap();
        // Empty terms list still builds a valid prompt
        assert!(p.contains("text"));
        assert!(p.contains("English"));
    }

    // ── Style template ───────────────────────────────────────────────────────

    #[test]
    fn style_prompt_english_target() {
        let o = PromptOpts {
            style: Some("formal academic".to_owned()),
            ..Default::default()
        };
        let p = build_prompt("text", "en", &TemplateType::Style, &o).unwrap();
        assert!(p.contains("[formal academic]"));
        assert!(p.contains("English"));
    }

    #[test]
    fn style_prompt_missing_style_errors() {
        let result = build_prompt("text", "en", &TemplateType::Style, &opts());
        assert!(matches!(result, Err(CoreError::MissingTemplateOption(_))));
    }

    // ── Personalization template ─────────────────────────────────────────────

    #[test]
    fn personalization_prompt_builds_task_list() {
        let o = PromptOpts {
            instructions: Some(vec![
                "Keep tone formal".to_owned(),
                "Use past tense".to_owned(),
            ]),
            ..Default::default()
        };
        let p = build_prompt("source text", "en", &TemplateType::Personalization, &o).unwrap();
        assert!(p.contains("1. Keep tone formal"));
        assert!(p.contains("2. Use past tense"));
        assert!(p.contains("Translate the [Source Text] into English"));
    }

    // ── Delimiters template ──────────────────────────────────────────────────

    #[test]
    fn delimiters_prompt_english() {
        let p = build_prompt("a|b|c", "en", &TemplateType::Delimiters, &opts()).unwrap();
        assert!(p.contains("exact same number of delimiters"));
        assert!(p.contains("a|b|c"));
    }

    #[test]
    fn delimiters_prompt_chinese() {
        let p = build_prompt("a|b", "zh", &TemplateType::Delimiters, &opts()).unwrap();
        assert!(p.contains("保留完全相同数量的分隔符"));
    }

    // ── Structured template ───────────────────────────────────────────────────

    #[test]
    fn structured_prompt_with_format() {
        let o = PromptOpts {
            format_type: Some("JSON".to_owned()),
            ..Default::default()
        };
        let p = build_prompt("{\"key\": \"val\"}", "en", &TemplateType::Structured, &o).unwrap();
        assert!(p.contains("JSON data"));
        assert!(p.contains("Structure Preservation"));
    }

    #[test]
    fn structured_prompt_default_format() {
        let p = build_prompt("data", "en", &TemplateType::Structured, &opts()).unwrap();
        assert!(p.contains("structured data"));
    }

    // ── ContextAware template ─────────────────────────────────────────────────

    #[test]
    fn context_prompt_english() {
        let o = PromptOpts {
            context: Some("Background about the topic.".to_owned()),
            ..Default::default()
        };
        let p = build_prompt("Translate me.", "en", &TemplateType::ContextAware, &o).unwrap();
        assert!(p.contains("Background about the topic."));
        assert!(p.contains("Translate me."));
        assert!(p.contains("Background Information"));
    }

    #[test]
    fn context_prompt_missing_context_errors() {
        let result = build_prompt("text", "en", &TemplateType::ContextAware, &opts());
        assert!(matches!(result, Err(CoreError::MissingTemplateOption(_))));
    }
}
