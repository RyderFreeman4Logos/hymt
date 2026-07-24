//! Canonical language metadata shared by every target-language consumer.
//!
//! Codes are looked up case-insensitively after normalizing `_` to `-`.

use crate::error::CoreError;

/// Broad script/language family traits used by target-aware behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageFamily {
    /// Targets whose output is detected with the shared CJK heuristic.
    Chinese,
    /// A target without Chinese-family handling.
    Other,
}

/// Canonical metadata for one supported translation target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageSpec {
    /// Canonical BCP-47 spelling persisted in cache keys and diagnostics.
    pub canonical_code: &'static str,
    /// Accepted normalized aliases, including the canonical code in lowercase.
    pub aliases: &'static [&'static str],
    /// Full language name for English prompt templates.
    pub english_name: &'static str,
    /// Full language name for Chinese prompt templates.
    pub chinese_name: &'static str,
    /// Family-specific behavior shared by detection and completeness checks.
    pub family: LanguageFamily,
    /// Filesystem-safe suffix for translated document names.
    pub output_suffix: &'static str,
}

/// Every target language accepted by Hy-MT2 prompt construction.
pub const LANGUAGE_SPECS: &[LanguageSpec] = &[
    LanguageSpec {
        canonical_code: "zh",
        aliases: &["zh", "zh-cn", "cn", "zh-hans"],
        english_name: "Chinese",
        chinese_name: "中文",
        family: LanguageFamily::Chinese,
        output_suffix: "zh-cn",
    },
    LanguageSpec {
        canonical_code: "zh-Hant",
        aliases: &["zh-hant", "zh-tw", "zh-hk", "zh-mo"],
        english_name: "Traditional Chinese",
        chinese_name: "繁体中文",
        family: LanguageFamily::Chinese,
        output_suffix: "zh-hant",
    },
    LanguageSpec {
        canonical_code: "en",
        aliases: &["en", "en-us", "en-gb", "en-au", "en-ca"],
        english_name: "English",
        chinese_name: "英语",
        family: LanguageFamily::Other,
        output_suffix: "en",
    },
    LanguageSpec {
        canonical_code: "fr",
        aliases: &["fr"],
        english_name: "French",
        chinese_name: "法语",
        family: LanguageFamily::Other,
        output_suffix: "fr",
    },
    LanguageSpec {
        canonical_code: "pt",
        aliases: &["pt"],
        english_name: "Portuguese",
        chinese_name: "葡萄牙语",
        family: LanguageFamily::Other,
        output_suffix: "pt",
    },
    LanguageSpec {
        canonical_code: "es",
        aliases: &["es"],
        english_name: "Spanish",
        chinese_name: "西班牙语",
        family: LanguageFamily::Other,
        output_suffix: "es",
    },
    LanguageSpec {
        canonical_code: "ja",
        aliases: &["ja", "ja-jp"],
        english_name: "Japanese",
        chinese_name: "日语",
        family: LanguageFamily::Other,
        output_suffix: "ja",
    },
    LanguageSpec {
        canonical_code: "tr",
        aliases: &["tr"],
        english_name: "Turkish",
        chinese_name: "土耳其语",
        family: LanguageFamily::Other,
        output_suffix: "tr",
    },
    LanguageSpec {
        canonical_code: "ru",
        aliases: &["ru"],
        english_name: "Russian",
        chinese_name: "俄语",
        family: LanguageFamily::Other,
        output_suffix: "ru",
    },
    LanguageSpec {
        canonical_code: "ar",
        aliases: &["ar"],
        english_name: "Arabic",
        chinese_name: "阿拉伯语",
        family: LanguageFamily::Other,
        output_suffix: "ar",
    },
    LanguageSpec {
        canonical_code: "ko",
        aliases: &["ko", "ko-kr"],
        english_name: "Korean",
        chinese_name: "韩语",
        family: LanguageFamily::Other,
        output_suffix: "ko",
    },
    LanguageSpec {
        canonical_code: "th",
        aliases: &["th"],
        english_name: "Thai",
        chinese_name: "泰语",
        family: LanguageFamily::Other,
        output_suffix: "th",
    },
    LanguageSpec {
        canonical_code: "it",
        aliases: &["it"],
        english_name: "Italian",
        chinese_name: "意大利语",
        family: LanguageFamily::Other,
        output_suffix: "it",
    },
    LanguageSpec {
        canonical_code: "de",
        aliases: &["de"],
        english_name: "German",
        chinese_name: "德语",
        family: LanguageFamily::Other,
        output_suffix: "de",
    },
    LanguageSpec {
        canonical_code: "vi",
        aliases: &["vi"],
        english_name: "Vietnamese",
        chinese_name: "越南语",
        family: LanguageFamily::Other,
        output_suffix: "vi",
    },
    LanguageSpec {
        canonical_code: "ms",
        aliases: &["ms"],
        english_name: "Malay",
        chinese_name: "马来语",
        family: LanguageFamily::Other,
        output_suffix: "ms",
    },
    LanguageSpec {
        canonical_code: "id",
        aliases: &["id"],
        english_name: "Indonesian",
        chinese_name: "印度尼西亚语",
        family: LanguageFamily::Other,
        output_suffix: "id",
    },
    LanguageSpec {
        canonical_code: "tl",
        aliases: &["tl"],
        english_name: "Tagalog",
        chinese_name: "他加禄语",
        family: LanguageFamily::Other,
        output_suffix: "tl",
    },
    LanguageSpec {
        canonical_code: "hi",
        aliases: &["hi"],
        english_name: "Hindi",
        chinese_name: "印地语",
        family: LanguageFamily::Other,
        output_suffix: "hi",
    },
    LanguageSpec {
        canonical_code: "pl",
        aliases: &["pl"],
        english_name: "Polish",
        chinese_name: "波兰语",
        family: LanguageFamily::Other,
        output_suffix: "pl",
    },
    LanguageSpec {
        canonical_code: "cs",
        aliases: &["cs"],
        english_name: "Czech",
        chinese_name: "捷克语",
        family: LanguageFamily::Other,
        output_suffix: "cs",
    },
    LanguageSpec {
        canonical_code: "nl",
        aliases: &["nl"],
        english_name: "Dutch",
        chinese_name: "荷兰语",
        family: LanguageFamily::Other,
        output_suffix: "nl",
    },
    LanguageSpec {
        canonical_code: "km",
        aliases: &["km"],
        english_name: "Khmer",
        chinese_name: "高棉语",
        family: LanguageFamily::Other,
        output_suffix: "km",
    },
    LanguageSpec {
        canonical_code: "my",
        aliases: &["my"],
        english_name: "Burmese",
        chinese_name: "缅甸语",
        family: LanguageFamily::Other,
        output_suffix: "my",
    },
    LanguageSpec {
        canonical_code: "fa",
        aliases: &["fa"],
        english_name: "Persian",
        chinese_name: "波斯语",
        family: LanguageFamily::Other,
        output_suffix: "fa",
    },
    LanguageSpec {
        canonical_code: "gu",
        aliases: &["gu"],
        english_name: "Gujarati",
        chinese_name: "古吉拉特语",
        family: LanguageFamily::Other,
        output_suffix: "gu",
    },
    LanguageSpec {
        canonical_code: "ur",
        aliases: &["ur"],
        english_name: "Urdu",
        chinese_name: "乌尔都语",
        family: LanguageFamily::Other,
        output_suffix: "ur",
    },
    LanguageSpec {
        canonical_code: "te",
        aliases: &["te"],
        english_name: "Telugu",
        chinese_name: "泰卢固语",
        family: LanguageFamily::Other,
        output_suffix: "te",
    },
    LanguageSpec {
        canonical_code: "mr",
        aliases: &["mr"],
        english_name: "Marathi",
        chinese_name: "马拉地语",
        family: LanguageFamily::Other,
        output_suffix: "mr",
    },
    LanguageSpec {
        canonical_code: "he",
        aliases: &["he"],
        english_name: "Hebrew",
        chinese_name: "希伯来语",
        family: LanguageFamily::Other,
        output_suffix: "he",
    },
    LanguageSpec {
        canonical_code: "bn",
        aliases: &["bn"],
        english_name: "Bengali",
        chinese_name: "孟加拉语",
        family: LanguageFamily::Other,
        output_suffix: "bn",
    },
    LanguageSpec {
        canonical_code: "ta",
        aliases: &["ta"],
        english_name: "Tamil",
        chinese_name: "泰米尔语",
        family: LanguageFamily::Other,
        output_suffix: "ta",
    },
    LanguageSpec {
        canonical_code: "uk",
        aliases: &["uk"],
        english_name: "Ukrainian",
        chinese_name: "乌克兰语",
        family: LanguageFamily::Other,
        output_suffix: "uk",
    },
    LanguageSpec {
        canonical_code: "bo",
        aliases: &["bo"],
        english_name: "Tibetan",
        chinese_name: "藏语",
        family: LanguageFamily::Other,
        output_suffix: "bo",
    },
    LanguageSpec {
        canonical_code: "kk",
        aliases: &["kk"],
        english_name: "Kazakh",
        chinese_name: "哈萨克语",
        family: LanguageFamily::Other,
        output_suffix: "kk",
    },
    LanguageSpec {
        canonical_code: "mn",
        aliases: &["mn"],
        english_name: "Mongolian",
        chinese_name: "蒙古语",
        family: LanguageFamily::Other,
        output_suffix: "mn",
    },
    LanguageSpec {
        canonical_code: "ug",
        aliases: &["ug"],
        english_name: "Uyghur",
        chinese_name: "维吾尔语",
        family: LanguageFamily::Other,
        output_suffix: "ug",
    },
    LanguageSpec {
        canonical_code: "yue",
        aliases: &["yue", "zh-yue"],
        english_name: "Cantonese",
        chinese_name: "粤语",
        family: LanguageFamily::Chinese,
        output_suffix: "yue",
    },
];

/// Look up a supported target without constructing a validation error.
pub fn language_spec_or_none(code: &str) -> Option<&'static LanguageSpec> {
    let normalized = normalize_alias(code);
    LANGUAGE_SPECS
        .iter()
        .find(|spec| spec.aliases.contains(&normalized.as_str()))
}

/// Return the canonical specification for a supported target code.
pub fn language_spec(code: &str) -> Result<&'static LanguageSpec, CoreError> {
    language_spec_or_none(code).ok_or_else(|| CoreError::UnsupportedLanguage {
        code: code.trim().to_owned(),
        supported: supported_language_codes(),
    })
}

/// Normalize a supported language code to its canonical BCP-47 spelling.
pub fn normalize_language_code(code: &str) -> Result<&'static str, CoreError> {
    Ok(language_spec(code)?.canonical_code)
}

/// Render the canonical target-code list for diagnostics and documentation.
pub fn supported_language_codes() -> String {
    LANGUAGE_SPECS
        .iter()
        .map(|spec| spec.canonical_code)
        .collect::<Vec<_>>()
        .join(", ")
}

fn normalize_alias(code: &str) -> String {
    code.trim().replace('_', "-").to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_normalize_to_canonical_bcp47_codes() {
        for (alias, canonical) in [
            ("zh", "zh"),
            ("zh-CN", "zh"),
            ("ZH_CN", "zh"),
            ("zh-Hant", "zh-Hant"),
            ("zh_hant", "zh-Hant"),
            ("ZH-tW", "zh-Hant"),
            ("zh-yue", "yue"),
        ] {
            assert_eq!(normalize_language_code(alias).unwrap(), canonical);
        }
    }

    #[test]
    fn every_spec_resolves_its_aliases_and_has_localized_names() {
        for spec in LANGUAGE_SPECS {
            assert!(!spec.english_name.is_empty());
            assert!(!spec.chinese_name.is_empty());
            assert!(!spec.output_suffix.is_empty());
            for alias in spec.aliases {
                assert_eq!(language_spec(alias).unwrap(), spec);
            }
        }
    }

    #[test]
    fn yue_is_a_chinese_family_language() {
        assert_eq!(
            language_spec("yue").unwrap().family,
            LanguageFamily::Chinese
        );
    }

    #[test]
    fn unknown_language_reports_supported_canonical_codes() {
        let error = language_spec("unknown").unwrap_err().to_string();
        assert!(error.contains("unsupported language 'unknown'"));
        assert!(error.contains("zh-Hant"));
        assert!(error.contains("yue"));
    }
}
