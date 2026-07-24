# HyMT reproducible translation benchmarks

This directory contains a fixed, versioned translation suite for comparing the four supported inference targets:

- Transformers
- vLLM
- llama.cpp with **Q6_K** GGUF
- llama.cpp with **Q4_K_M** GGUF

The corpus is deliberately checked in. Generated reports are local artifacts under `benchmarks/results/` and are ignored by Git.

## Quick start

Run the complete deterministic mock suite (the default and safe for CI):

```bash
just benchmark
```

It validates `corpus/v1.json`, all system/sampler definitions, metrics, report generation, and the decision gates without a model server. It writes:

- `benchmarks/results/latest/results.json` — versioned machine-readable records, summaries, gates, and reproducibility data
- `benchmarks/results/latest/report.md` — a review-friendly table and gate decision report

Validate schemas only, without creating backend records:

```bash
just benchmark --dry-run
```

## Live execution (explicitly opt in)

Live requests are intentionally blocked unless `HYMT_BENCHMARK_LIVE=1`. The runner uses direct OpenAI-compatible `/v1/chat/completions` requests rather than the HyMT cache client, adds `Cache-Control: no-store`, and supplies a unique `X-HyMT-Benchmark-Run` header. This prevents the benchmark from silently measuring HyMT's output cache.

Set the endpoint and all reproducibility metadata variables for every selected system. Live runs reject missing model identity/revision, tokenizer revision, backend version, and (for llama.cpp targets) GGUF SHA-256 values before making requests:

```bash
export HYMT_BENCHMARK_LIVE=1
export HYMT_BENCH_TRANSFORMERS_URL=https://transformers.example/v1/chat/completions
export HYMT_BENCH_TRANSFORMERS_MODEL=/models/hymt
export HYMT_BENCH_TRANSFORMERS_MODEL_REVISION=<git-or-hub-revision>
export HYMT_BENCH_TRANSFORMERS_TOKENIZER_REVISION=<git-or-hub-revision>
export HYMT_BENCH_TRANSFORMERS_VERSION=<build-version>
# Repeat for VLLM, LLAMA_CPP_Q6_K, and LLAMA_CPP_Q4_K.
# Supply HYMT_BENCH_LLAMA_CPP_Q{4_K_M,6_K}_GGUF_SHA256 for both GGUF systems.

just benchmark --live --output-dir benchmarks/results/$(date -u +%Y%m%dT%H%M%SZ)
```

The runner uses streaming responses when the backend serves SSE, so it reports end-to-end latency, first-token latency, and output characters/sec. Non-streaming endpoints still report end-to-end latency and throughput; first-token latency is explicitly `n/a`, never synthesized.

Use a previous JSON result for compatible per-system/per-sampler chrF regression checks. The runner rejects baselines with a different result schema, corpus or prompt schema/hash, mode, or selected-system identity metadata:

```bash
just benchmark --live --baseline benchmarks/results/baseline/results.json
```

A non-passing gate makes the CLI exit non-zero after writing both artifacts. Limit a troubleshooting execution without weakening the full-suite configuration:

```bash
just benchmark --live --system llama-cpp-q6-k
```

## Corpus and metrics

`corpus/v1.json` has more than 50 examples across Chinese↔English prose, additional language pairs, UI text, long prose, terminology/style/context, CLI help, Markdown, JSON/YAML/TOML, mixed-language and multi-segment content, and adversarial repetition/truncation cases. Every entry declares:

- source, reference when available, language pair, category, and template type
- natural-language invariants
- exact tokens that must survive (placeholders, URLs, flags, delimiters, keys, code, and IDs)
- a structured format when parse validation applies

The standard metrics are:

- **chrF**: character n-grams 1–6, F-beta=2, corpus-weighted by system and sampler.
- **Exact preservation**: required tokens present / required tokens.
- **Structured parse**: `serde_json`, `serde_yaml`, or `toml` parser success.
- **Truncation/completeness**: empty output, `finish_reason=length`, malformed structured output, or trailing `...`.
- **Source-language residue**: script heuristic after declared preservation tokens are removed. It is a diagnostic, not a semantic-quality substitute.
- **Latency**: p50 full-response latency, p50 first-token latency when streamed, and output characters/sec.

COMET/xCOMET is intentionally optional: its model dependencies are not bundled in the Rust workspace. The JSON artifact labels it `not-configured`, allowing an environment-specific evaluator to append semantic scores without making base CI download a metric model.

## Decision gates

Thresholds are versioned in `decision-gates.toml` and evaluated for every system/sampler summary:

1. complete request coverage for every selected system/sampler; any live backend request error fails the run's quality gates;
2. absolute chrF and optional same-system/sampler baseline regression;
3. exact preservation, parse success, truncation/completeness, and residue ceilings;
4. Q4_K_M chrF tradeoff against Q6_K (and Q4 throughput ratio when live measurements exist);
5. sampler compatibility: max chrF spread, preservation floor, and truncation ceiling for service defaults, min-p on/off, repeat-last-n 64/full, and top-p variants.

Any threshold change is a reviewable corpus/gate change, not an ad-hoc dashboard setting.

## CI and scheduled jobs

Fast pull-request CI should use the hermetic modes:

```yaml
- run: just benchmark --dry-run
- run: just benchmark
```

A self-hosted scheduled job with access to the exact models and GGUF files should run `--live`, archive both artifacts, and pass the prior accepted `results.json` as `--baseline`. Set each model/tokenizer revision, GGUF SHA-256, backend build version, and endpoint in the job environment; do not store credentials or raw endpoint URLs in committed reports.

The report records benchmark commit, corpus SHA-256 and schema, prompt schema, cache policy, hardware/OS, model/tokenizer revisions, GGUF hash, backend version/build, endpoint availability, and resolved `/props` JSON when the backend exposes it.
