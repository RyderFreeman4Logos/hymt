use std::path::PathBuf;

use hymt_bench::{
    load_corpus, load_decision_gates, run_benchmark, validate_corpus, RunMode, RunOptions,
};

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn mock_options(output_dir: PathBuf) -> RunOptions {
    RunOptions {
        corpus_path: repo_path("benchmarks/corpus/v1.json"),
        systems_path: repo_path("benchmarks/systems.toml"),
        gates_path: repo_path("benchmarks/decision-gates.toml"),
        output_dir,
        mode: RunMode::Mock,
        baseline_path: None,
        system_ids: Vec::new(),
    }
}

#[test]
fn corpus_is_versioned_diverse_and_valid() {
    let corpus = load_corpus(&repo_path("benchmarks/corpus/v1.json")).unwrap();
    validate_corpus(&corpus).unwrap();

    assert!(corpus.examples.len() >= 50);
    assert_eq!(corpus.schema_version, "hymt-benchmark-corpus/v1");
    for category in [
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
    ] {
        assert!(
            corpus
                .examples
                .iter()
                .any(|example| example.category == category),
            "missing benchmark category {category}"
        );
    }
    assert!(corpus
        .examples
        .iter()
        .all(|example| !example.expected_preserved_tokens.is_empty()));
}

#[test]
fn mock_run_emits_machine_and_human_reports_with_reproducibility_metadata() {
    let output = tempfile::tempdir().unwrap();
    let report = run_benchmark(&mock_options(output.path().to_path_buf())).unwrap();

    assert!(report.records.len() >= 50 * 4);
    assert_eq!(report.metadata.mode, "mock");
    assert_eq!(report.metadata.cache.status, "disabled");
    assert!(!report.metadata.benchmark_commit.is_empty());
    assert!(report.metadata.corpus_sha256.len() >= 32);
    assert!(
        report.gates.iter().all(|gate| gate.passed),
        "{:#?}",
        report.gates
    );
    assert!(report
        .records
        .iter()
        .all(|record| record.metrics.chrf == Some(1.0)));
    assert!(report
        .records
        .iter()
        .all(|record| record.metrics.preservation_rate == 1.0));
    assert!(report.records.iter().all(|record| {
        record.metrics.structured_parse_success != Some(false)
            && !record.metrics.truncated_or_incomplete
    }));
    assert!(report.records.iter().all(|record| {
        record
            .metrics
            .source_language_residue_rate
            .is_none_or(|rate| rate <= 0.2)
    }));

    let json = output.path().join("results.json");
    let markdown = output.path().join("report.md");
    assert!(json.is_file());
    assert!(markdown.is_file());
    let markdown = std::fs::read_to_string(markdown).unwrap();
    assert!(markdown.contains("# HyMT translation benchmark"));
    assert!(markdown.contains("## Decision gates"));
    assert!(markdown.contains("first-token latency"));
}

#[test]
fn dry_run_validates_the_harness_without_executing_backends() {
    let output = tempfile::tempdir().unwrap();
    let report = run_benchmark(&RunOptions {
        mode: RunMode::DryRun,
        ..mock_options(output.path().to_path_buf())
    })
    .unwrap();

    assert_eq!(report.metadata.mode, "dry-run");
    assert!(report.records.is_empty());
    assert!(report.gates.iter().all(|gate| gate.passed));
    assert!(output.path().join("results.json").is_file());
}

#[test]
fn decision_gates_are_versioned_and_cover_required_tradeoffs() {
    let gates = load_decision_gates(&repo_path("benchmarks/decision-gates.toml")).unwrap();
    assert_eq!(gates.schema_version, "hymt-benchmark-gates/v1");
    assert!(gates.quality.min_chrf > 0.0);
    assert_eq!(gates.preservation.min_rate, 1.0);
    assert!(gates.truncation.max_rate < 0.1);
    assert!(gates.quantization.q4_vs_q6_max_chrf_drop > 0.0);
    assert!(gates.sampler.max_chrf_spread > 0.0);
}
