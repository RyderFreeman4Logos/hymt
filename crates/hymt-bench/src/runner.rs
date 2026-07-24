use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::corpus::{load_corpus, validate_corpus, Corpus, Example};
use crate::model::{
    score_example, summarize, BenchmarkReport, CacheMetadata, DecisionGates, GateResult,
    HostMetadata, MetricSummary, ReproducibilityMetadata, RunRecord, SamplerVariant,
    SystemDefinition, SystemMetadata, SystemsConfig, TimingMetrics,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunMode {
    Mock,
    DryRun,
    Live,
}

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub corpus_path: PathBuf,
    pub systems_path: PathBuf,
    pub gates_path: PathBuf,
    pub output_dir: PathBuf,
    pub mode: RunMode,
    pub baseline_path: Option<PathBuf>,
    pub system_ids: Vec<String>,
}

pub fn load_decision_gates(path: &Path) -> Result<DecisionGates> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("read decision gates {}", path.display()))?;
    let gates: DecisionGates = toml::from_str(&source)
        .with_context(|| format!("parse decision gates {}", path.display()))?;
    validate_gates(&gates)?;
    Ok(gates)
}

pub fn run_benchmark(options: &RunOptions) -> Result<BenchmarkReport> {
    let corpus = load_corpus(&options.corpus_path)?;
    validate_corpus(&corpus)?;
    let systems = load_systems(&options.systems_path)?;
    validate_systems(&systems, &corpus)?;
    let gates = load_decision_gates(&options.gates_path)?;
    let selected = select_systems(&systems, &options.system_ids)?;
    let mut metadata =
        reproducibility_metadata(&options.mode, &options.corpus_path, &corpus, &selected)?;

    let records = match options.mode {
        RunMode::DryRun => Vec::new(),
        RunMode::Mock => mock_records(&corpus, &selected),
        RunMode::Live => {
            if env::var("HYMT_BENCHMARK_LIVE").as_deref() != Ok("1") {
                bail!("live benchmark execution requires HYMT_BENCHMARK_LIVE=1; use --mock or --dry-run otherwise");
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let (records, live_metadata) =
                runtime.block_on(live_records(&corpus, &selected, &metadata.run_id))?;
            metadata.systems = live_metadata;
            records
        }
    };

    let summaries = summarize(&records);
    let baseline = options
        .baseline_path
        .as_deref()
        .map(load_baseline)
        .transpose()?;
    let gates = if options.mode == RunMode::DryRun {
        vec![GateResult {
            name: "configuration-valid".into(),
            passed: true,
            status: "not-evaluated".into(),
            observed: None,
            threshold: None,
            message: "corpus, systems, and gate schemas validated; no backend was executed".into(),
        }]
    } else {
        evaluate_gates(&summaries, &gates, baseline.as_ref(), &selected)
    };
    let report = BenchmarkReport {
        schema_version: "hymt-benchmark-results/v1".into(),
        metadata,
        records,
        summaries,
        gates,
    };
    write_report(&report, &options.output_dir)?;
    Ok(report)
}

fn load_systems(path: &Path) -> Result<SystemsConfig> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("read systems config {}", path.display()))?;
    toml::from_str(&source).with_context(|| format!("parse systems config {}", path.display()))
}

fn validate_systems(systems: &SystemsConfig, corpus: &Corpus) -> Result<()> {
    if systems.schema_version != "hymt-benchmark-systems/v1" {
        bail!("unsupported systems schema: {}", systems.schema_version);
    }
    if systems.prompt_schema_version != corpus.prompt_schema_version {
        bail!("systems prompt schema does not match corpus prompt schema");
    }
    let required = ["transformers", "vllm", "llama-cpp-q6-k", "llama-cpp-q4-k-m"];
    for id in required {
        let Some(system) = systems.systems.iter().find(|system| system.id == id) else {
            bail!("systems configuration is missing {id}");
        };
        if system.sampler_variants.len() < 5 {
            bail!("system {id} requires all five sampler variants");
        }
    }
    Ok(())
}

fn validate_gates(gates: &DecisionGates) -> Result<()> {
    if gates.schema_version != "hymt-benchmark-gates/v1" {
        bail!("unsupported gate schema: {}", gates.schema_version);
    }
    for (name, value) in [
        ("quality.min_chrf", gates.quality.min_chrf),
        ("preservation.min_rate", gates.preservation.min_rate),
        ("parse.min_rate", gates.parse.min_rate),
        ("truncation.max_rate", gates.truncation.max_rate),
        ("residue.max_rate", gates.residue.max_rate),
    ] {
        if !(0.0..=1.0).contains(&value) {
            bail!("gate {name} must be within 0..=1");
        }
    }
    Ok(())
}

fn select_systems<'a>(
    systems: &'a SystemsConfig,
    ids: &[String],
) -> Result<Vec<&'a SystemDefinition>> {
    let selected: Vec<_> = systems
        .systems
        .iter()
        .filter(|system| ids.is_empty() || ids.iter().any(|id| id == &system.id))
        .collect();
    if selected.is_empty() {
        bail!("no configured systems matched requested filter");
    }
    for id in ids {
        if !selected.iter().any(|system| &system.id == id) {
            bail!("unknown benchmark system {id}");
        }
    }
    Ok(selected)
}

fn mock_records(corpus: &Corpus, systems: &[&SystemDefinition]) -> Vec<RunRecord> {
    systems
        .iter()
        .flat_map(|system| {
            system.sampler_variants.iter().flat_map(move |sampler| {
                corpus.examples.iter().map(move |example| {
                    let output = example
                        .reference
                        .clone()
                        .unwrap_or_else(|| example.source.clone());
                    let timing = TimingMetrics::default();
                    RunRecord {
                        system_id: system.id.clone(),
                        backend: system.backend.clone(),
                        quantization: system.quantization.clone(),
                        sampler_id: sampler.id.clone(),
                        example_id: example.id.clone(),
                        metrics: score_example(example, &output, Some("stop")),
                        output,
                        finish_reason: Some("stop".into()),
                        timing,
                        error: None,
                    }
                })
            })
        })
        .collect()
}

async fn live_records(
    corpus: &Corpus,
    systems: &[&SystemDefinition],
    run_id: &str,
) -> Result<(Vec<RunRecord>, Vec<SystemMetadata>)> {
    let client = reqwest::Client::builder().build()?;
    let mut records = Vec::new();
    let mut metadata = Vec::new();
    for system in systems {
        let endpoint = env::var(&system.endpoint_env)
            .with_context(|| format!("{} is required for live execution", system.endpoint_env))?;
        let system_metadata = live_system_metadata(&client, system, &endpoint).await;
        for sampler in &system.sampler_variants {
            for example in &corpus.examples {
                let result =
                    execute_request(&client, system, sampler, example, &endpoint, run_id).await;
                let (output, finish_reason, timing, error) = match result {
                    Ok(response) => (
                        response.output,
                        response.finish_reason,
                        response.timing,
                        None,
                    ),
                    Err(error) => (
                        String::new(),
                        None,
                        TimingMetrics::default(),
                        Some(error.to_string()),
                    ),
                };
                let metrics = score_example(example, &output, finish_reason.as_deref());
                records.push(RunRecord {
                    system_id: system.id.clone(),
                    backend: system.backend.clone(),
                    quantization: system.quantization.clone(),
                    sampler_id: sampler.id.clone(),
                    example_id: example.id.clone(),
                    output,
                    finish_reason,
                    metrics,
                    timing,
                    error,
                });
            }
        }
        metadata.push(system_metadata);
    }
    Ok((records, metadata))
}

struct LiveResponse {
    output: String,
    finish_reason: Option<String>,
    timing: TimingMetrics,
}

async fn execute_request(
    client: &reqwest::Client,
    system: &SystemDefinition,
    sampler: &SamplerVariant,
    example: &Example,
    endpoint: &str,
    run_id: &str,
) -> Result<LiveResponse> {
    let mut body = Map::new();
    body.insert(
        "model".into(),
        Value::String(env::var(&system.model_env).unwrap_or_else(|_| "benchmark-model".into())),
    );
    body.insert("stream".into(), Value::Bool(true));
    body.insert(
        "messages".into(),
        json!([{"role":"user", "content": benchmark_prompt(example)}]),
    );
    if sampler.client_overrides {
        if let Some(top_p) = sampler.top_p {
            body.insert("top_p".into(), json!(top_p));
        }
        if system.supports_min_p {
            if let Some(min_p) = sampler.min_p {
                body.insert("min_p".into(), json!(min_p));
            }
        }
        if system.supports_repeat_last_n {
            if let Some(repeat_last_n) = sampler.repeat_last_n {
                body.insert("repeat_last_n".into(), json!(repeat_last_n));
            }
        }
    }
    let start = Instant::now();
    let response = client
        .post(endpoint)
        .header("Cache-Control", "no-store")
        .header("X-HyMT-Benchmark-Run", run_id)
        .json(&body)
        .send()
        .await?;
    if !response.status().is_success() {
        bail!("backend returned HTTP {}", response.status());
    }
    let streaming = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"));
    if !streaming {
        let value: Value = response.json().await?;
        let output = completion_content(&value)
            .ok_or_else(|| anyhow!("backend response did not include completion content"))?;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        return Ok(LiveResponse {
            timing: timing_for(&output, elapsed, None),
            output,
            finish_reason: completion_finish_reason(&value),
        });
    }
    let mut stream = response.bytes_stream();
    let mut pending = String::new();
    let mut output = String::new();
    let mut finish_reason = None;
    let mut first_token_ms = None;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        let chunk = String::from_utf8_lossy(&bytes);
        pending.push_str(&chunk);
        while let Some(position) = pending.find('\n') {
            let line = pending.drain(..=position).collect::<String>();
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                continue;
            }
            let value: Value =
                serde_json::from_str(data).context("parse streamed backend event")?;
            if let Some(content) = stream_content(&value) {
                if !content.is_empty() && first_token_ms.is_none() {
                    first_token_ms = Some(start.elapsed().as_secs_f64() * 1000.0);
                }
                output.push_str(&content);
            }
            if let Some(reason) = completion_finish_reason(&value) {
                finish_reason = Some(reason);
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    Ok(LiveResponse {
        timing: timing_for(&output, elapsed, first_token_ms),
        output,
        finish_reason,
    })
}

fn benchmark_prompt(example: &Example) -> String {
    let (source, target) = example
        .language_pair
        .split_once('-')
        .unwrap_or(("source", "target"));
    format!("Translate from {source} to {target}. Return only the translation. Preserve these tokens exactly: {}.\n\n{}", example.expected_preserved_tokens.join(", "), example.source)
}
fn completion_content(value: &Value) -> Option<String> {
    value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| stream_content(value))
}
fn stream_content(value: &Value) -> Option<String> {
    value
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}
fn completion_finish_reason(value: &Value) -> Option<String> {
    value
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}
fn timing_for(output: &str, latency_ms: f64, first_token_latency_ms: Option<f64>) -> TimingMetrics {
    TimingMetrics {
        latency_ms: Some(latency_ms),
        first_token_latency_ms,
        throughput_chars_per_second: (latency_ms > 0.0)
            .then(|| output.chars().count() as f64 / (latency_ms / 1000.0)),
    }
}

async fn live_system_metadata(
    client: &reqwest::Client,
    system: &SystemDefinition,
    endpoint: &str,
) -> SystemMetadata {
    let without_completion = endpoint
        .strip_suffix("/chat/completions")
        .unwrap_or(endpoint);
    let base = without_completion
        .strip_suffix("/v1")
        .unwrap_or(without_completion)
        .trim_end_matches('/');
    let props_url = format!("{base}{}", system.props_path);
    let resolved_props = match client
        .get(props_url)
        .header("Cache-Control", "no-store")
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response.json::<Value>().await.ok(),
        _ => None,
    };
    system_metadata(system, true, resolved_props)
}

fn system_metadata(
    system: &SystemDefinition,
    endpoint_configured: bool,
    resolved_props: Option<Value>,
) -> SystemMetadata {
    SystemMetadata {
        id: system.id.clone(),
        backend: system.backend.clone(),
        quantization: system.quantization.clone(),
        endpoint_configured,
        model: env_value(&system.model_env),
        model_revision: env_value(&system.model_revision_env),
        tokenizer_revision: env_value(&system.tokenizer_revision_env),
        gguf_sha256: system.gguf_sha256_env.as_deref().and_then(env_value),
        backend_version: env_value(&system.backend_version_env),
        resolved_props,
    }
}
fn env_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn reproducibility_metadata(
    mode: &RunMode,
    corpus_path: &Path,
    corpus: &Corpus,
    systems: &[&SystemDefinition],
) -> Result<ReproducibilityMetadata> {
    let bytes = fs::read(corpus_path)?;
    let mut optional_metrics = BTreeMap::new();
    optional_metrics.insert(
        "comet_xcomet".into(),
        "not-configured (external optional evaluator)".into(),
    );
    Ok(ReproducibilityMetadata {
        mode: match mode {
            RunMode::Mock => "mock",
            RunMode::DryRun => "dry-run",
            RunMode::Live => "live",
        }
        .into(),
        run_id: format!(
            "{}-{}",
            Utc::now().format("%Y%m%dT%H%M%SZ"),
            std::process::id()
        ),
        timestamp_utc: Utc::now().to_rfc3339(),
        benchmark_commit: git_commit(),
        corpus_sha256: hex::encode(Sha256::digest(bytes)),
        corpus_schema_version: corpus.schema_version.clone(),
        prompt_schema_version: corpus.prompt_schema_version.clone(),
        cache: CacheMetadata {
            status: "disabled".into(),
            method: "no HyMT cache client; Cache-Control: no-store and unique run header".into(),
        },
        host: host_metadata(),
        systems: systems
            .iter()
            .map(|system| system_metadata(system, env::var(&system.endpoint_env).is_ok(), None))
            .collect(),
        optional_metrics,
    })
}
fn git_commit() -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|| "unavailable".into())
}
fn host_metadata() -> HostMetadata {
    let cpu = fs::read_to_string("/proc/cpuinfo").ok().and_then(|source| {
        source
            .lines()
            .find_map(|line| line.strip_prefix("model name\t: ").map(ToOwned::to_owned))
    });
    let memory_available_kib = fs::read_to_string("/proc/meminfo").ok().and_then(|source| {
        source.lines().find_map(|line| {
            line.strip_prefix("MemAvailable:")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse().ok())
        })
    });
    HostMetadata {
        os: env::consts::OS.into(),
        architecture: env::consts::ARCH.into(),
        cpu,
        memory_available_kib,
    }
}

fn evaluate_gates(
    summaries: &[MetricSummary],
    gates: &DecisionGates,
    baseline: Option<&BenchmarkReport>,
    systems: &[&SystemDefinition],
) -> Vec<GateResult> {
    let summary_values = |selector: fn(&MetricSummary) -> Option<f64>| {
        summaries.iter().filter_map(selector).collect::<Vec<_>>()
    };
    let all_at_least = |values: &[f64], threshold| {
        !values.is_empty() && values.iter().all(|value| *value >= threshold)
    };
    let all_at_most = |values: &[f64], threshold| {
        !values.is_empty() && values.iter().all(|value| *value <= threshold)
    };
    let chrf = summary_values(|summary| summary.chrf);
    let preservation = summaries
        .iter()
        .map(|summary| summary.preservation_rate)
        .collect::<Vec<_>>();
    let parse = summary_values(|summary| summary.structured_parse_rate);
    let truncation = summaries
        .iter()
        .map(|summary| summary.truncation_rate)
        .collect::<Vec<_>>();
    let residue = summary_values(|summary| summary.source_language_residue_rate);
    let mut results = vec![
        gate(
            "quality-chrf",
            all_at_least(&chrf, gates.quality.min_chrf),
            min_value(&chrf),
            gates.quality.min_chrf,
            "absolute corpus-weighted chrF",
        ),
        gate(
            "preservation",
            all_at_least(&preservation, gates.preservation.min_rate),
            min_value(&preservation),
            gates.preservation.min_rate,
            "exact required-token preservation",
        ),
        gate(
            "structured-parse",
            all_at_least(&parse, gates.parse.min_rate),
            min_value(&parse),
            gates.parse.min_rate,
            "JSON/YAML/TOML parse rate",
        ),
        gate(
            "truncation",
            all_at_most(&truncation, gates.truncation.max_rate),
            max_value(&truncation),
            gates.truncation.max_rate,
            "truncation or completeness failure rate",
        ),
        gate(
            "source-language-residue",
            all_at_most(&residue, gates.residue.max_rate),
            max_value(&residue),
            gates.residue.max_rate,
            "script-detection residue heuristic",
        ),
    ];
    results.push(baseline_gate(
        summaries,
        baseline,
        gates.quality.max_baseline_chrf_regression,
    ));
    results.push(quantization_gate(summaries, systems, gates));
    results.push(quantization_throughput_gate(summaries, systems, gates));
    results.push(sampler_gate(summaries, gates));
    results
}

fn gate(
    name: &str,
    passed: bool,
    observed: Option<f64>,
    threshold: f64,
    message: &str,
) -> GateResult {
    GateResult {
        name: name.into(),
        passed,
        status: if passed { "evaluated" } else { "failed" }.into(),
        observed,
        threshold: Some(threshold),
        message: message.into(),
    }
}
fn min_value(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::min)
}
fn max_value(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::max)
}
fn baseline_gate(
    summaries: &[MetricSummary],
    baseline: Option<&BenchmarkReport>,
    max_regression: f64,
) -> GateResult {
    let Some(baseline) = baseline else {
        return GateResult {
            name: "baseline-quality-regression".into(),
            passed: true,
            status: "not-evaluated".into(),
            observed: None,
            threshold: Some(max_regression),
            message: "no baseline supplied".into(),
        };
    };
    let baseline_values: HashMap<_, _> = baseline
        .summaries
        .iter()
        .filter_map(|summary| {
            summary
                .chrf
                .map(|value| ((&summary.system_id, &summary.sampler_id), value))
        })
        .collect();
    let deltas: Vec<_> = summaries
        .iter()
        .filter_map(|summary| {
            summary.chrf.and_then(|value| {
                baseline_values
                    .get(&(&summary.system_id, &summary.sampler_id))
                    .map(|base| value - base)
            })
        })
        .collect();
    gate(
        "baseline-quality-regression",
        !deltas.is_empty() && deltas.iter().all(|delta| *delta >= -max_regression),
        min_value(&deltas),
        max_regression,
        "chrF delta versus same system and sampler baseline",
    )
}
fn quantization_gate(
    summaries: &[MetricSummary],
    systems: &[&SystemDefinition],
    gates: &DecisionGates,
) -> GateResult {
    if !systems.iter().any(|system| system.id == "llama-cpp-q4-k-m")
        || !systems.iter().any(|system| system.id == "llama-cpp-q6-k")
    {
        return GateResult {
            name: "q4-vs-q6-tradeoff".into(),
            passed: true,
            status: "not-evaluated".into(),
            observed: None,
            threshold: Some(gates.quantization.q4_vs_q6_max_chrf_drop),
            message: "quantization pair not selected".into(),
        };
    }
    let q4 = aggregate_system(summaries, "llama-cpp-q4-k-m", |summary| summary.chrf);
    let q6 = aggregate_system(summaries, "llama-cpp-q6-k", |summary| summary.chrf);
    let drop = q6.zip(q4).map(|(q6, q4)| q6 - q4);
    gate(
        "q4-vs-q6-tradeoff",
        drop.is_some_and(|drop| drop <= gates.quantization.q4_vs_q6_max_chrf_drop),
        drop,
        gates.quantization.q4_vs_q6_max_chrf_drop,
        "Q4_K_M chrF drop relative to Q6_K",
    )
}
fn quantization_throughput_gate(
    summaries: &[MetricSummary],
    systems: &[&SystemDefinition],
    gates: &DecisionGates,
) -> GateResult {
    if !systems.iter().any(|system| system.id == "llama-cpp-q4-k-m")
        || !systems.iter().any(|system| system.id == "llama-cpp-q6-k")
    {
        return GateResult {
            name: "q4-vs-q6-throughput".into(),
            passed: true,
            status: "not-evaluated".into(),
            observed: None,
            threshold: Some(gates.quantization.q4_min_throughput_ratio),
            message: "quantization pair not selected".into(),
        };
    }
    let q4 = aggregate_system(summaries, "llama-cpp-q4-k-m", |summary| {
        summary.throughput_chars_per_second
    });
    let q6 = aggregate_system(summaries, "llama-cpp-q6-k", |summary| {
        summary.throughput_chars_per_second
    });
    let ratio = q4.zip(q6).and_then(|(q4, q6)| (q6 > 0.0).then(|| q4 / q6));
    match ratio {
        Some(value) => gate(
            "q4-vs-q6-throughput",
            value >= gates.quantization.q4_min_throughput_ratio,
            Some(value),
            gates.quantization.q4_min_throughput_ratio,
            "Q4_K_M / Q6_K output-throughput ratio",
        ),
        None => GateResult {
            name: "q4-vs-q6-throughput".into(),
            passed: true,
            status: "not-evaluated".into(),
            observed: None,
            threshold: Some(gates.quantization.q4_min_throughput_ratio),
            message: "throughput unavailable (mock or non-timed run)".into(),
        },
    }
}

fn sampler_gate(summaries: &[MetricSummary], gates: &DecisionGates) -> GateResult {
    let mut grouped: HashMap<&str, Vec<&MetricSummary>> = HashMap::new();
    for summary in summaries {
        grouped.entry(&summary.system_id).or_default().push(summary);
    }
    let spread: Vec<_> = grouped
        .values()
        .filter_map(|group| {
            let values: Vec<_> = group.iter().filter_map(|summary| summary.chrf).collect();
            max_value(&values)
                .zip(min_value(&values))
                .map(|(max, min)| max - min)
        })
        .collect();
    let preserve = summaries
        .iter()
        .map(|summary| summary.preservation_rate)
        .collect::<Vec<_>>();
    let truncation = summaries
        .iter()
        .map(|summary| summary.truncation_rate)
        .collect::<Vec<_>>();
    let passed = !spread.is_empty()
        && max_value(&spread).is_some_and(|value| value <= gates.sampler.max_chrf_spread)
        && preserve
            .iter()
            .all(|value| *value >= gates.sampler.min_preservation_rate)
        && truncation
            .iter()
            .all(|value| *value <= gates.sampler.max_truncation_rate);
    gate(
        "sampler-compatibility",
        passed,
        max_value(&spread),
        gates.sampler.max_chrf_spread,
        "sampler chrF spread plus preservation/truncation compatibility",
    )
}
fn aggregate_system(
    summaries: &[MetricSummary],
    system: &str,
    selector: fn(&MetricSummary) -> Option<f64>,
) -> Option<f64> {
    let values: Vec<_> = summaries
        .iter()
        .filter(|summary| summary.system_id == system)
        .filter_map(selector)
        .collect();
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn load_baseline(path: &Path) -> Result<BenchmarkReport> {
    serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("parse baseline {}", path.display()))
}

fn write_report(report: &BenchmarkReport, output_dir: &Path) -> Result<()> {
    fs::create_dir_all(output_dir)
        .with_context(|| format!("create output directory {}", output_dir.display()))?;
    fs::write(
        output_dir.join("results.json"),
        serde_json::to_vec_pretty(report)?,
    )?;
    fs::write(output_dir.join("report.md"), markdown_report(report))?;
    Ok(())
}

fn markdown_report(report: &BenchmarkReport) -> String {
    let mut markdown = format!("# HyMT translation benchmark\n\nRun `{}` in **{}** mode. Corpus SHA-256: `{}`.\n\n## Summary\n\n| system | sampler | samples | chrF | preservation | parse | truncation | residue | p50 latency (ms) | p50 first-token latency (ms) | chars/s |\n| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n", report.metadata.run_id, report.metadata.mode, report.metadata.corpus_sha256);
    for summary in &report.summaries {
        markdown.push_str(&format!(
            "| {} ({}) | {} | {} | {} | {:.3} | {} | {:.3} | {} | {} | {} | {} |\n",
            summary.system_id,
            summary.quantization,
            summary.sampler_id,
            summary.samples,
            opt(summary.chrf),
            summary.preservation_rate,
            opt(summary.structured_parse_rate),
            summary.truncation_rate,
            opt(summary.source_language_residue_rate),
            opt(summary.latency_p50_ms),
            opt(summary.first_token_latency_p50_ms),
            opt(summary.throughput_chars_per_second)
        ));
    }
    markdown.push_str("\n## Decision gates\n\n| gate | status | observed | threshold | detail |\n| --- | --- | ---: | ---: | --- |\n");
    for gate in &report.gates {
        markdown.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            gate.name,
            gate.status,
            opt(gate.observed),
            opt(gate.threshold),
            gate.message
        ));
    }
    markdown.push_str("\n## Reproducibility metadata\n\n");
    markdown.push_str(&format!(
        "- Commit: `{}`\n- Prompt schema: `{}`\n- Cache: {} ({})\n- Host: {} / {}\n",
        report.metadata.benchmark_commit,
        report.metadata.prompt_schema_version,
        report.metadata.cache.status,
        report.metadata.cache.method,
        report.metadata.host.os,
        report.metadata.host.architecture
    ));
    for system in &report.metadata.systems {
        markdown.push_str(&format!("- {}: backend={}, quantization={}, model_revision={}, tokenizer_revision={}, GGUF SHA-256={}, backend_version={}, props={}\n", system.id, system.backend, system.quantization, system.model_revision.as_deref().unwrap_or("unavailable"), system.tokenizer_revision.as_deref().unwrap_or("unavailable"), system.gguf_sha256.as_deref().unwrap_or("unavailable"), system.backend_version.as_deref().unwrap_or("unavailable"), if system.resolved_props.is_some() { "captured" } else { "unavailable" }));
    }
    markdown
}
fn opt(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.3}"))
        .unwrap_or_else(|| "n/a".into())
}
