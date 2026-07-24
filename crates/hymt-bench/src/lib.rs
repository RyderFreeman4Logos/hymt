mod corpus;
mod model;
mod runner;

pub use corpus::{load_corpus, validate_corpus, Corpus, Example};
pub use model::{chrf, BenchmarkReport, DecisionGates, GateResult, MetricSummary};
pub use runner::{load_decision_gates, run_benchmark, RunMode, RunOptions};
