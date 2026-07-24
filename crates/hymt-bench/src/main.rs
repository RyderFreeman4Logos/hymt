use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use hymt_bench::{run_benchmark, RunMode, RunOptions};

#[derive(Debug, Parser)]
#[command(
    name = "hymt-bench",
    about = "Reproducible HyMT translation benchmarks"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the fixed cross-backend benchmark suite.
    Run(RunArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Validate corpus/configuration only; do not execute or mock a backend.
    #[arg(long, conflicts_with_all = ["mock", "live"])]
    dry_run: bool,
    /// Produce deterministic reference-based mock results without contacting backends.
    #[arg(long, conflicts_with = "live")]
    mock: bool,
    /// Contact configured backend endpoints; requires HYMT_BENCHMARK_LIVE=1.
    #[arg(long, conflicts_with = "mock")]
    live: bool,
    #[arg(long, default_value = "benchmarks/corpus/v1.json")]
    corpus: PathBuf,
    #[arg(long, default_value = "benchmarks/systems.toml")]
    systems: PathBuf,
    #[arg(long, default_value = "benchmarks/decision-gates.toml")]
    gates: PathBuf,
    #[arg(long, default_value = "benchmarks/results/latest")]
    output_dir: PathBuf,
    /// Compare compatible system/sampler summaries against an earlier results.json file.
    #[arg(long)]
    baseline: Option<PathBuf>,
    /// Restrict the run to one or more comma-separated system IDs.
    #[arg(long, value_delimiter = ',')]
    system: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => {
            let mode = if args.dry_run {
                RunMode::DryRun
            } else if args.live {
                RunMode::Live
            } else {
                RunMode::Mock
            };
            let report = run_benchmark(&RunOptions {
                corpus_path: args.corpus,
                systems_path: args.systems,
                gates_path: args.gates,
                output_dir: args.output_dir,
                mode,
                baseline_path: args.baseline,
                system_ids: args.system,
            })?;
            let failures = report.gates.iter().filter(|gate| !gate.passed).count();
            println!(
                "benchmark mode={} records={} gate_failures={}",
                report.metadata.mode,
                report.records.len(),
                failures
            );
            if failures > 0 {
                anyhow::bail!(
                    "benchmark decision gates failed; inspect report.md and results.json"
                );
            }
        }
    }
    Ok(())
}
