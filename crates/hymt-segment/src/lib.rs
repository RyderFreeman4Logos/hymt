pub mod error;
mod split;
#[cfg(test)]
mod tests;

pub use error::SegmentError;

use std::path::PathBuf;

#[cfg(feature = "tokenizer")]
use tokenizers::Tokenizer as HfTokenizer;

const TOKENIZER_REPO: &str = "tencent/Hy-MT2-7B";
const TOKENIZER_FILENAME: &str = "tokenizer.json";

/// Fallback token estimate: ~4 UTF-8 bytes per token (matches Python's _estimate_token_count).
fn estimate_token_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.len().div_ceil(4).max(1)
}

fn tokenizer_cache_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join(".cache/hymt/tokenizer")
}

pub fn tokenizer_path() -> PathBuf {
    tokenizer_cache_dir().join(TOKENIZER_FILENAME)
}

/// Download the HuggingFace tokenizer to `~/.cache/hymt/tokenizer/tokenizer.json`.
#[cfg(feature = "tokenizer")]
pub fn ensure_tokenizer(force_download: bool) -> Result<PathBuf, SegmentError> {
    let dest = tokenizer_path();
    if dest.exists() && !force_download {
        return Ok(dest);
    }
    std::fs::create_dir_all(tokenizer_cache_dir())?;
    let api = hf_hub::api::sync::Api::new().map_err(|e| SegmentError::Download(e.to_string()))?;
    let downloaded = api
        .model(TOKENIZER_REPO.to_string())
        .get(TOKENIZER_FILENAME)
        .map_err(|e| SegmentError::Download(e.to_string()))?;
    std::fs::copy(&downloaded, &dest)?;
    Ok(dest)
}

#[cfg(not(feature = "tokenizer"))]
pub fn ensure_tokenizer(_force_download: bool) -> Result<PathBuf, SegmentError> {
    Err(SegmentError::Download(
        "tokenizer feature not compiled in".into(),
    ))
}

pub fn has_tokenizer_support() -> bool {
    cfg!(feature = "tokenizer")
}

pub struct Segmenter {
    #[cfg(feature = "tokenizer")]
    tokenizer: Option<HfTokenizer>,
    #[cfg(not(feature = "tokenizer"))]
    _marker: std::marker::PhantomData<()>,
}

impl Segmenter {
    pub fn new(tokenizer_path: Option<PathBuf>) -> Result<Self, SegmentError> {
        #[cfg(feature = "tokenizer")]
        {
            let tokenizer = tokenizer_path
                .map(|p| {
                    HfTokenizer::from_file(p).map_err(|e| SegmentError::Tokenizer(e.to_string()))
                })
                .transpose()?;
            Ok(Self { tokenizer })
        }
        #[cfg(not(feature = "tokenizer"))]
        {
            let _ = tokenizer_path;
            Ok(Self {
                _marker: std::marker::PhantomData,
            })
        }
    }

    /// Create a `Segmenter` without a tokenizer; uses character-count fallback.
    pub fn fallback() -> Self {
        #[cfg(feature = "tokenizer")]
        return Self { tokenizer: None };
        #[cfg(not(feature = "tokenizer"))]
        Self {
            _marker: std::marker::PhantomData,
        }
    }

    pub fn count_tokens(&self, text: &str) -> usize {
        #[cfg(feature = "tokenizer")]
        if let Some(tok) = &self.tokenizer {
            return tok
                .encode(text, false)
                .map(|enc| enc.get_ids().len())
                .unwrap_or_else(|_| estimate_token_count(text));
        }
        estimate_token_count(text)
    }

    pub fn segment(&self, text: &str, max_tokens: usize) -> Result<Vec<String>, SegmentError> {
        if max_tokens == 0 {
            return Err(SegmentError::InvalidMaxTokens);
        }
        if text.is_empty() {
            return Ok(vec![]);
        }
        if self.count_tokens(text) <= max_tokens {
            return Ok(vec![text.to_owned()]);
        }

        let mut units: Vec<String> = Vec::new();

        for paragraph in split::split_paragraphs(text) {
            if self.count_tokens(&paragraph) <= max_tokens {
                units.push(paragraph);
                continue;
            }
            for sentence in split::split_sentences(&paragraph) {
                if self.count_tokens(&sentence) <= max_tokens {
                    units.push(sentence);
                    continue;
                }
                for clause in split::split_clauses(&sentence) {
                    if self.count_tokens(&clause) <= max_tokens {
                        units.push(clause);
                        continue;
                    }
                    units.extend(self.split_word_or_character(&clause, max_tokens)?);
                }
            }
        }

        self.pack_units(units, max_tokens)
    }

    fn split_word_or_character(
        &self,
        text: &str,
        max_tokens: usize,
    ) -> Result<Vec<String>, SegmentError> {
        let word_units: Vec<String> = split::split_on_whitespace(text);

        if word_units.len() > 1 {
            let mut chunks: Vec<String> = Vec::new();
            for unit in word_units {
                if self.count_tokens(&unit) <= max_tokens {
                    chunks.push(unit);
                } else {
                    chunks.extend(self.split_characters(&unit, max_tokens)?);
                }
            }
            return self.pack_units(chunks, max_tokens);
        }

        self.split_characters(text, max_tokens)
    }

    fn split_characters(&self, text: &str, max_tokens: usize) -> Result<Vec<String>, SegmentError> {
        let mut chunks: Vec<String> = Vec::new();
        let mut current = String::new();

        for ch in text.chars() {
            let ch_str = ch.to_string();
            if self.count_tokens(&ch_str) > max_tokens {
                return Err(SegmentError::MaxTokensTooSmall);
            }
            let candidate = format!("{current}{ch}");
            if !current.is_empty() && self.count_tokens(&candidate) > max_tokens {
                chunks.push(std::mem::take(&mut current));
                current = ch_str;
            } else {
                current = candidate;
            }
        }

        if !current.is_empty() {
            chunks.push(current);
        }

        Ok(chunks)
    }

    fn pack_units(
        &self,
        units: Vec<String>,
        max_tokens: usize,
    ) -> Result<Vec<String>, SegmentError> {
        let mut segments: Vec<String> = Vec::new();
        let mut current = String::new();

        for unit in units {
            if unit.is_empty() {
                continue;
            }
            if self.count_tokens(&unit) > max_tokens {
                return Err(SegmentError::UnitExceedsMaxTokens);
            }
            let candidate = format!("{current}{unit}");
            if !current.is_empty() && self.count_tokens(&candidate) > max_tokens {
                segments.push(std::mem::take(&mut current));
                current = unit;
            } else {
                current = candidate;
            }
        }

        if !current.is_empty() {
            segments.push(current);
        }

        Ok(segments)
    }
}

/// Create a `Segmenter`, downloading the tokenizer if needed.
/// Falls back to character-count mode on any error.
pub fn create_segmenter(force_download: bool) -> Segmenter {
    if !has_tokenizer_support() {
        return Segmenter::fallback();
    }
    match ensure_tokenizer(force_download) {
        Ok(path) => Segmenter::new(Some(path)).unwrap_or_else(|_| Segmenter::fallback()),
        Err(_) => Segmenter::fallback(),
    }
}
