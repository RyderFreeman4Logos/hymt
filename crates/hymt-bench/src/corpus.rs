use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Corpus {
    pub schema_version: String,
    pub prompt_schema_version: String,
    pub examples: Vec<Example>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Example {
    pub id: String,
    pub category: String,
    pub language_pair: String,
    pub template_type: String,
    pub source: String,
    pub reference: Option<String>,
    #[serde(default)]
    pub invariants: Vec<String>,
    #[serde(default)]
    pub expected_preserved_tokens: Vec<String>,
    #[serde(default)]
    pub structured_format: Option<String>,
}

pub fn load_corpus(path: &Path) -> Result<Corpus> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("read benchmark corpus {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parse benchmark corpus {}", path.display()))
}

pub fn validate_corpus(corpus: &Corpus) -> Result<()> {
    if corpus.schema_version != "hymt-benchmark-corpus/v1" {
        bail!(
            "unsupported corpus schema version: {}",
            corpus.schema_version
        );
    }
    if corpus.prompt_schema_version.trim().is_empty() {
        bail!("corpus prompt_schema_version must be set");
    }
    if corpus.examples.len() < 50 {
        bail!(
            "benchmark corpus needs at least 50 examples, found {}",
            corpus.examples.len()
        );
    }

    let required_categories = [
        "zh-en-prose",
        "additional-language-pairs",
        "ui-strings",
        "long-paragraphs",
        "terminology",
        "style",
        "context-aware",
        "cli-help",
        "markdown",
        "structured-data",
        "mixed-language",
        "multi-segment",
        "adversarial-repetition-truncation",
    ];
    let categories: HashSet<&str> = corpus
        .examples
        .iter()
        .map(|example| example.category.as_str())
        .collect();
    for category in required_categories {
        if !categories.contains(category) {
            bail!("benchmark corpus is missing category {category}");
        }
    }

    let mut ids = HashSet::new();
    for example in &corpus.examples {
        if example.id.trim().is_empty()
            || example.source.trim().is_empty()
            || example.language_pair.trim().is_empty()
            || example.template_type.trim().is_empty()
        {
            bail!("example has a required empty field: {}", example.id);
        }
        if !ids.insert(example.id.as_str()) {
            bail!("duplicate example id: {}", example.id);
        }
        if example.expected_preserved_tokens.is_empty() {
            bail!("example {} has no expected_preserved_tokens", example.id);
        }
        for token in &example.expected_preserved_tokens {
            if token.is_empty() || !example.source.contains(token) {
                bail!(
                    "example {} declares invalid preserved token {token:?}",
                    example.id
                );
            }
            if let Some(reference) = &example.reference {
                if !reference.contains(token) {
                    bail!("reference for {} does not preserve {token:?}", example.id);
                }
            }
        }
        if let Some(format) = &example.structured_format {
            if !matches!(format.as_str(), "json" | "yaml" | "toml") {
                bail!(
                    "example {} uses unsupported structured format {format}",
                    example.id
                );
            }
        }
    }
    Ok(())
}
