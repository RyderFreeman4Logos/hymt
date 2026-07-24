pub mod error;
mod split;
#[cfg(test)]
mod tests;

pub use error::SegmentError;

use std::path::PathBuf;

use hymt_core::model_profile::ModelProfile;

#[cfg(feature = "tokenizer")]
use tokenizers::Tokenizer as HfTokenizer;

const TOKENIZER_FILENAME: &str = "tokenizer.json";

/// Fallback token estimate: ~4 UTF-8 bytes per token (matches Python's _estimate_token_count).
fn estimate_token_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.len().div_ceil(4).max(1)
}

fn tokenizer_cache_dir(profile: ModelProfile) -> Option<PathBuf> {
    profile.tokenizer().map(|_| {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join(".cache/hymt/tokenizer")
            .join(profile.id())
    })
}

/// Cache path for a profile's pinned tokenizer, if it has one.
///
/// The profile id is part of the path, so a prior 7B download can never be
/// silently reused for the 1.8B or 30B-A3B profiles.
pub fn tokenizer_path(profile: ModelProfile) -> Option<PathBuf> {
    tokenizer_cache_dir(profile).map(|dir| dir.join(TOKENIZER_FILENAME))
}

/// Download the selected profile's pinned tokenizer into the profile cache.
#[cfg(feature = "tokenizer")]
pub fn ensure_tokenizer(
    profile: ModelProfile,
    force_download: bool,
) -> Result<PathBuf, SegmentError> {
    let source = profile.tokenizer().ok_or_else(|| {
        SegmentError::Download(
            "generic model profile has no tested tokenizer; set [endpoint].profile".into(),
        )
    })?;
    let dest = tokenizer_path(profile).ok_or_else(|| {
        SegmentError::Download("tested model profile is missing a tokenizer cache path".into())
    })?;
    if dest.exists() && !force_download {
        return Ok(dest);
    }
    let cache_dir = dest.parent().ok_or_else(|| {
        SegmentError::Download("tokenizer cache path has no parent directory".into())
    })?;
    std::fs::create_dir_all(cache_dir)?;
    let api = hf_hub::api::sync::Api::new().map_err(|e| SegmentError::Download(e.to_string()))?;
    let downloaded = api
        .repo(hf_hub::Repo::with_revision(
            source.repo.to_owned(),
            hf_hub::RepoType::Model,
            source.revision.to_owned(),
        ))
        .get(TOKENIZER_FILENAME)
        .map_err(|e| SegmentError::Download(e.to_string()))?;
    std::fs::copy(&downloaded, &dest)?;
    Ok(dest)
}

#[cfg(not(feature = "tokenizer"))]
pub fn ensure_tokenizer(
    _profile: ModelProfile,
    _force_download: bool,
) -> Result<PathBuf, SegmentError> {
    Err(SegmentError::Download(
        "tokenizer feature not compiled in".into(),
    ))
}

pub fn has_tokenizer_support() -> bool {
    cfg!(feature = "tokenizer")
}

/// Create a segmenter using a cached tokenizer for the selected profile.
///
/// Missing tokenizers intentionally retain the existing character-estimate
/// fallback; `hymt tokenizer download` performs the explicit network fetch.
pub fn create_segmenter(profile: ModelProfile) -> Result<Segmenter, SegmentError> {
    match tokenizer_path(profile).filter(|path| path.exists()) {
        Some(path) => Segmenter::new(Some(path)),
        None => Ok(Segmenter::fallback()),
    }
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

        for block in split::split_markdown_blocks(text) {
            match block {
                // Fenced code and tables are atomic even when oversized.
                split::MarkdownBlock::FencedCode(s) | split::MarkdownBlock::Table(s) => {
                    units.push(s);
                }
                // Blockquotes are split at line boundaries when oversized,
                // preserving the `>` prefix on every resulting segment.
                split::MarkdownBlock::Blockquote(s) => {
                    if self.count_tokens(&s) <= max_tokens {
                        units.push(s);
                    } else {
                        units.extend(self.split_blockquote(&s, max_tokens));
                    }
                }
                // Lists split at top-level item boundaries when oversized.
                split::MarkdownBlock::List(s) => {
                    if self.count_tokens(&s) <= max_tokens {
                        units.push(s);
                    } else {
                        for item in split::split_list_items(&s) {
                            self.add_text_units(&mut units, &item, max_tokens)?;
                        }
                    }
                }
                // Normal paragraphs use the sentence→clause→word→char hierarchy.
                split::MarkdownBlock::Normal(s) => {
                    self.add_text_units(&mut units, &s, max_tokens)?;
                }
            }
        }

        self.pack_units(units, max_tokens)
    }

    /// Recursively split a normal text chunk using sentence → clause → word → char.
    fn add_text_units(
        &self,
        units: &mut Vec<String>,
        text: &str,
        max_tokens: usize,
    ) -> Result<(), SegmentError> {
        if self.count_tokens(text) <= max_tokens {
            units.push(text.to_owned());
            return Ok(());
        }
        for sentence in split::split_sentences(text) {
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
        Ok(())
    }

    /// Split an oversized blockquote at line boundaries, keeping `>` prefixes.
    fn split_blockquote(&self, text: &str, max_tokens: usize) -> Vec<String> {
        let mut segments: Vec<String> = Vec::new();
        let mut current = String::new();

        for line in text.split_inclusive('\n') {
            let candidate = format!("{current}{line}");
            if !current.is_empty() && self.count_tokens(&candidate) > max_tokens {
                segments.push(std::mem::take(&mut current));
                current = line.to_owned();
            } else {
                current = candidate;
            }
        }

        if !current.is_empty() {
            segments.push(current);
        }

        segments
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
            // Protected blocks (fenced code, tables) may be legitimately oversized.
            // Emit them as their own segment rather than erroring.
            if self.count_tokens(&unit) > max_tokens {
                if !current.is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
                segments.push(unit);
                continue;
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
