use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::corpus::{Corpus, Example};

#[derive(Clone, Debug, Deserialize)]
pub struct SystemsConfig {
    pub schema_version: String,
    pub prompt_schema_version: String,
    pub systems: Vec<SystemDefinition>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SystemDefinition {
    pub id: String,
    pub backend: String,
    pub quantization: String,
    pub endpoint_env: String,
    pub model_env: String,
    pub model_revision_env: String,
    pub tokenizer_revision_env: String,
    #[serde(default)]
    pub gguf_sha256_env: Option<String>,
    pub backend_version_env: String,
    #[serde(default)]
    pub supports_min_p: bool,
    #[serde(default)]
    pub supports_repeat_last_n: bool,
    #[serde(default = "default_props_path")]
    pub props_path: String,
    pub sampler_variants: Vec<SamplerVariant>,
}

fn default_props_path() -> String {
    "/props".to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SamplerVariant {
    pub id: String,
    #[serde(default)]
    pub client_overrides: bool,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub min_p: Option<f64>,
    #[serde(default)]
    pub repeat_last_n: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DecisionGates {
    pub schema_version: String,
    pub quality: QualityGate,
    pub preservation: PreservationGate,
    pub parse: ParseGate,
    pub truncation: TruncationGate,
    pub residue: ResidueGate,
    pub quantization: QuantizationGate,
    pub sampler: SamplerGate,
}

#[derive(Clone, Debug, Deserialize)]
pub struct QualityGate {
    pub min_chrf: f64,
    pub max_baseline_chrf_regression: f64,
}
#[derive(Clone, Debug, Deserialize)]
pub struct PreservationGate {
    pub min_rate: f64,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ParseGate {
    pub min_rate: f64,
}
#[derive(Clone, Debug, Deserialize)]
pub struct TruncationGate {
    pub max_rate: f64,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ResidueGate {
    pub max_rate: f64,
}
#[derive(Clone, Debug, Deserialize)]
pub struct QuantizationGate {
    pub q4_vs_q6_max_chrf_drop: f64,
    pub q4_min_throughput_ratio: f64,
}
#[derive(Clone, Debug, Deserialize)]
pub struct SamplerGate {
    pub max_chrf_spread: f64,
    pub min_preservation_rate: f64,
    pub max_truncation_rate: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub schema_version: String,
    pub metadata: ReproducibilityMetadata,
    pub records: Vec<RunRecord>,
    pub summaries: Vec<MetricSummary>,
    pub gates: Vec<GateResult>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReproducibilityMetadata {
    pub mode: String,
    pub run_id: String,
    pub timestamp_utc: String,
    pub benchmark_commit: String,
    pub corpus_sha256: String,
    pub corpus_schema_version: String,
    pub prompt_schema_version: String,
    pub cache: CacheMetadata,
    pub host: HostMetadata,
    pub systems: Vec<SystemMetadata>,
    pub optional_metrics: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheMetadata {
    pub status: String,
    pub method: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostMetadata {
    pub os: String,
    pub architecture: String,
    pub cpu: Option<String>,
    pub memory_available_kib: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemMetadata {
    pub id: String,
    pub backend: String,
    pub quantization: String,
    pub endpoint_configured: bool,
    pub model: Option<String>,
    pub model_revision: Option<String>,
    pub tokenizer_revision: Option<String>,
    pub gguf_sha256: Option<String>,
    pub backend_version: Option<String>,
    pub resolved_props: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub system_id: String,
    pub backend: String,
    pub quantization: String,
    pub sampler_id: String,
    pub example_id: String,
    pub output: String,
    pub finish_reason: Option<String>,
    pub metrics: ExampleMetrics,
    pub timing: TimingMetrics,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExampleMetrics {
    pub chrf: Option<f64>,
    pub preservation_rate: f64,
    pub structured_parse_success: Option<bool>,
    pub truncated_or_incomplete: bool,
    pub source_language_residue_rate: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TimingMetrics {
    pub latency_ms: Option<f64>,
    pub first_token_latency_ms: Option<f64>,
    pub throughput_chars_per_second: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetricSummary {
    pub system_id: String,
    pub backend: String,
    pub quantization: String,
    pub sampler_id: String,
    pub samples: usize,
    pub chrf: Option<f64>,
    pub preservation_rate: f64,
    pub structured_parse_rate: Option<f64>,
    pub truncation_rate: f64,
    pub source_language_residue_rate: Option<f64>,
    pub latency_p50_ms: Option<f64>,
    pub first_token_latency_p50_ms: Option<f64>,
    pub throughput_chars_per_second: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GateResult {
    pub name: String,
    pub passed: bool,
    pub status: String,
    pub observed: Option<f64>,
    pub threshold: Option<f64>,
    pub message: String,
}

pub fn summarize(records: &[RunRecord], corpus: &Corpus) -> Vec<MetricSummary> {
    let examples_by_id: HashMap<_, _> = corpus
        .examples
        .iter()
        .map(|example| (example.id.as_str(), example))
        .collect();
    let mut groups: HashMap<(&str, &str), Vec<&RunRecord>> = HashMap::new();
    for record in records.iter().filter(|record| record.error.is_none()) {
        groups
            .entry((&record.system_id, &record.sampler_id))
            .or_default()
            .push(record);
    }
    let mut summaries: Vec<_> = groups
        .into_values()
        .map(|records| {
            let first = records[0];
            let chrf_pairs: Vec<_> = records
                .iter()
                .filter_map(|record| {
                    examples_by_id
                        .get(record.example_id.as_str())
                        .and_then(|example| example.reference.as_deref())
                        .map(|reference| (reference, record.output.as_str()))
                })
                .collect();
            MetricSummary {
                system_id: first.system_id.clone(),
                backend: first.backend.clone(),
                quantization: first.quantization.clone(),
                sampler_id: first.sampler_id.clone(),
                samples: records.len(),
                chrf: (!chrf_pairs.is_empty()).then(|| corpus_chrf(chrf_pairs)),
                preservation_rate: mean(
                    records
                        .iter()
                        .map(|record| record.metrics.preservation_rate),
                ),
                structured_parse_rate: mean_optional(records.iter().filter_map(|record| {
                    record
                        .metrics
                        .structured_parse_success
                        .map(|parsed| if parsed { 1.0 } else { 0.0 })
                })),
                truncation_rate: mean(records.iter().map(|record| {
                    if record.metrics.truncated_or_incomplete {
                        1.0
                    } else {
                        0.0
                    }
                })),
                source_language_residue_rate: mean_optional(
                    records
                        .iter()
                        .filter_map(|record| record.metrics.source_language_residue_rate),
                ),
                latency_p50_ms: median(
                    records
                        .iter()
                        .filter_map(|record| record.timing.latency_ms)
                        .collect(),
                ),
                first_token_latency_p50_ms: median(
                    records
                        .iter()
                        .filter_map(|record| record.timing.first_token_latency_ms)
                        .collect(),
                ),
                throughput_chars_per_second: mean_optional(
                    records
                        .iter()
                        .filter_map(|record| record.timing.throughput_chars_per_second),
                ),
            }
        })
        .collect();
    summaries.sort_by(|left, right| {
        (&left.system_id, &left.sampler_id).cmp(&(&right.system_id, &right.sampler_id))
    });
    summaries
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let values: Vec<_> = values.collect();
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}
fn mean_optional(values: impl Iterator<Item = f64>) -> Option<f64> {
    let values: Vec<_> = values.collect();
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}
fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    })
}

pub fn score_example(
    example: &Example,
    output: &str,
    finish_reason: Option<&str>,
) -> ExampleMetrics {
    let preservation_rate = if example.expected_preserved_tokens.is_empty() {
        1.0
    } else {
        example
            .expected_preserved_tokens
            .iter()
            .filter(|token| output.contains(token.as_str()))
            .count() as f64
            / example.expected_preserved_tokens.len() as f64
    };
    let structured_parse_success =
        example
            .structured_format
            .as_deref()
            .map(|format| match format {
                "json" => serde_json::from_str::<serde_json::Value>(output).is_ok(),
                "yaml" => serde_yaml::from_str::<serde_yaml::Value>(output).is_ok(),
                "toml" => toml::from_str::<toml::Value>(output).is_ok(),
                _ => false,
            });
    let truncated_or_incomplete = output.trim().is_empty()
        || matches!(finish_reason, Some("length"))
        || structured_parse_success == Some(false)
        || output.trim_end().ends_with("...");
    ExampleMetrics {
        chrf: example
            .reference
            .as_deref()
            .map(|reference| chrf(reference, output)),
        preservation_rate,
        structured_parse_success,
        truncated_or_incomplete,
        source_language_residue_rate: source_language_residue(example, output),
    }
}

/// Character n-gram F-score (n=1..6, beta=2), using character multiset overlap.
pub fn chrf(reference: &str, hypothesis: &str) -> f64 {
    corpus_chrf([(reference, hypothesis)])
}

/// Corpus-level character n-gram F-score (n=1..6, beta=2).
///
/// Counts are accumulated by n-gram order across every comparable pair before
/// computing each order's F-score. This intentionally differs from averaging
/// per-example chrF values, which would give short and long examples equal
/// weight.
pub fn corpus_chrf<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> f64 {
    let mut matched = [0_usize; 6];
    let mut reference_total = [0_usize; 6];
    let mut hypothesis_total = [0_usize; 6];
    for (reference, hypothesis) in pairs {
        for n in 1..=6 {
            let reference = char_ngrams(reference, n);
            let hypothesis = char_ngrams(hypothesis, n);
            let index = n - 1;
            reference_total[index] += reference.values().sum::<usize>();
            hypothesis_total[index] += hypothesis.values().sum::<usize>();
            matched[index] += hypothesis
                .iter()
                .map(|(gram, count)| count.min(reference.get(gram).unwrap_or(&0)))
                .sum::<usize>();
        }
    }

    let scores: Vec<_> = (0..6)
        .filter_map(|index| {
            let reference = reference_total[index];
            let hypothesis = hypothesis_total[index];
            (reference != 0 && hypothesis != 0).then(|| {
                let precision = matched[index] as f64 / hypothesis as f64;
                let recall = matched[index] as f64 / reference as f64;
                let beta_squared = 4.0;
                if precision + recall == 0.0 {
                    0.0
                } else {
                    (1.0 + beta_squared) * precision * recall / (beta_squared * precision + recall)
                }
            })
        })
        .collect();
    if scores.is_empty() {
        0.0
    } else {
        scores.iter().sum::<f64>() / scores.len() as f64
    }
}

fn char_ngrams(input: &str, n: usize) -> HashMap<Vec<char>, usize> {
    let characters: Vec<_> = input.chars().collect();
    let mut result = HashMap::new();
    for gram in characters.windows(n) {
        *result.entry(gram.to_vec()).or_insert(0) += 1;
    }
    result
}

fn source_language_residue(example: &Example, output: &str) -> Option<f64> {
    let (_, target) = example.language_pair.split_once('-')?;
    let stripped = example
        .expected_preserved_tokens
        .iter()
        .fold(output.to_owned(), |text, token| text.replace(token, ""));
    let visible: Vec<char> = stripped
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if visible.is_empty() {
        return Some(0.0);
    }
    let residue = match target {
        "en" => visible.iter().filter(|character| matches!(**character as u32, 0x4e00..=0x9fff | 0x3040..=0x30ff | 0xac00..=0xd7af)).count(),
        "zh" | "ja" => visible.iter().filter(|character| character.is_ascii_alphabetic()).count(),
        _ => return None,
    };
    Some(residue as f64 / visible.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_chrf_aggregates_ngram_counts_instead_of_example_scores() {
        let pairs = [("aaaaaa", "aaaaaa"), ("bbbbbbbbbbbb", "xxxxxxxxxxxx")];

        let score = corpus_chrf(pairs);
        let per_example_mean =
            (chrf("aaaaaa", "aaaaaa") + chrf("bbbbbbbbbbbb", "xxxxxxxxxxxx")) / 2.0;

        assert!((score - 0.251_091_269_841_269_84).abs() < 1e-12);
        assert!((score - per_example_mean).abs() > 0.1);
    }
}
