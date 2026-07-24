mod zsh_plugin;

#[cfg(feature = "telegram")]
mod telegram;

use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, CommandFactory, Parser, Subcommand};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use hymt_cache::history::HistoryDB;
use hymt_client::TranslationClient;
use hymt_core::config::HotConfig;
use hymt_core::language::{resolve_target_language, DocumentTranslationPolicy};
use hymt_core::language_spec::{language_spec_or_none, LanguageFamily};
use hymt_core::model_profile::ModelProfile;
use hymt_core::templates::{looks_like_cli_help_source, PromptOpts, TemplateType};
use hymt_segment::Segmenter;
use hymt_translate::batch::{
    build_batch_plan, run_batch_translation, show_batch_preview, BatchPlanOpts,
};
use hymt_translate::doc_translate::{run_doc_translation, DocTranslationOpts};
use hymt_translate::docs::{run_info_command, run_man_command, ManInfoOpts};
use hymt_translate::exec_wrapper::{run_exec_command, ExecCommandOpts};
use hymt_translate::precache::run_precache;
use hymt_translate::translate::{
    plan_translation, translate_file, translate_text, translate_text_stream_with_mode,
    write_translation_output, StreamEvent, StreamOutputMode, TranslationCtx, TranslationOutcome,
    TranslationPlan,
};

// ── Known subcommand names (for smart routing) ────────────────────────────────

const KNOWN_SUBCOMMANDS: &[&str] = &[
    "config",
    "backend",
    "man",
    "info",
    "exec",
    "tokenizer",
    "estimate",
    "batch",
    "history",
    "recall",
    "translate-doc",
    "telegram",
    "help",
    "--help",
    "-h",
    "--version",
    "-V",
];

// ── Top-level CLI ─────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "hymt",
    about = "Hy-MT2 translation CLI",
    long_about = "Translate text, files, man pages, and command output via Hy-MT2.\n\
                  Run without a subcommand to translate positional text or a file.",
    version
)]
struct Cli {
    /// Target language (default: from config / auto-detected)
    #[arg(short = 'l', long, global = true)]
    lang: Option<String>,

    /// Accept all prompts without confirmation
    #[arg(short = 'y', long, global = true)]
    yes: bool,

    /// Show translation plan without executing
    #[arg(long, global = true)]
    plan: bool,

    /// Enable streaming output (default: on)
    #[arg(
        long,
        global = true,
        default_value_t = true,
        overrides_with = "no_stream"
    )]
    stream: bool,

    /// Disable streaming
    #[arg(long = "no-stream", alias = "no-streaming", global = true)]
    no_stream: bool,

    /// Override `[translation].concurrency` for this run (minimum 1)
    #[arg(long, global = true, value_name = "N")]
    concurrency: Option<u32>,

    /// Log per-chunk pipeline timestamps on stderr
    #[arg(long = "debug-chunk-timing", global = true)]
    debug_chunk_timing: bool,

    /// Submit every non-code paragraph, including text already in the target language
    #[arg(long, global = true, conflicts_with = "no_language_detection")]
    force_translate_all: bool,

    /// Disable target-language detection and preserve no target-language paragraphs
    #[arg(long, global = true, conflicts_with = "force_translate_all")]
    no_language_detection: bool,

    /// Write top-level text/stdin/file translation to this file
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// Show progress indicators
    #[arg(
        long,
        global = true,
        default_value_t = true,
        overrides_with = "no_progress"
    )]
    progress: bool,

    #[arg(long = "no-progress", global = true, hide = true)]
    no_progress: bool,

    /// Keep exit 0 when completeness falls back to best attempt
    #[arg(long = "warn-only-completeness", global = true)]
    warn_only_completeness: bool,

    /// Translation template
    #[arg(long, value_enum, default_value_t = TemplateArg::Default, global = true)]
    template: TemplateArg,

    /// Domain-specific terms as `source=translation` pairs (repeatable)
    #[arg(long, global = true, value_name = "SRC=TGT")]
    term: Vec<String>,

    /// Translation style hint
    #[arg(long, global = true)]
    style: Option<String>,

    /// Additional instructions for the model
    #[arg(long, global = true)]
    instructions: Option<String>,

    /// Output format type
    #[arg(long, global = true)]
    format_type: Option<String>,

    /// Contextual information for the model
    #[arg(long = "context", global = true)]
    ctx: Option<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

// ── Template argument (mirrors TemplateType) ──────────────────────────────────

#[derive(Clone, clap::ValueEnum)]
enum TemplateArg {
    Default,
    Terminology,
    Style,
    Personalization,
    Delimiters,
    Structured,
    ContextAware,
}

impl From<&TemplateArg> for TemplateType {
    fn from(t: &TemplateArg) -> Self {
        match t {
            TemplateArg::Default => TemplateType::Default,
            TemplateArg::Terminology => TemplateType::Terminology,
            TemplateArg::Style => TemplateType::Style,
            TemplateArg::Personalization => TemplateType::Personalization,
            TemplateArg::Delimiters => TemplateType::Delimiters,
            TemplateArg::Structured => TemplateType::Structured,
            TemplateArg::ContextAware => TemplateType::ContextAware,
        }
    }
}

// ── Subcommands ───────────────────────────────────────────────────────────────

#[derive(Subcommand)]
enum Cmd {
    /// Show, locate, or edit the hymt configuration file
    Config(ConfigArgs),
    /// Inspect configured backend values and live service capabilities
    Backend(BackendArgs),
    /// Translate a man page and display it in a pager
    Man(ManArgs),
    /// Translate an info page and display it in a pager
    Info(InfoArgs),
    /// Run a command and translate its output
    Exec(ExecArgs),
    /// Manage the tokenizer model
    Tokenizer(TokenizerArgs),
    /// Estimate translation time for a source character count
    Estimate(EstimateArgs),
    /// Translate all text files in a directory
    Batch(BatchArgs),
    /// Show translation history
    History(HistoryArgs),
    /// Recall a previous translation by position
    Recall(RecallArgs),
    /// Translate Markdown documents
    #[command(name = "translate-doc")]
    TranslateDoc(TranslateDocArgs),
    /// Run the Telegram CN↔EN translation bot (long-poll until Ctrl+C)
    Telegram(TelegramArgs),
    /// Translate positional text or a file (default command)
    #[command(external_subcommand)]
    Text(Vec<String>),
}

// ── config ────────────────────────────────────────────────────────────────────

#[derive(Args)]
struct ConfigArgs {
    #[command(subcommand)]
    action: Option<ConfigAction>,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show the current configuration
    Show,
    /// Print the path to the config file
    Path,
    /// Open the config file in $EDITOR
    Edit,
}

// ── backend ──────────────────────────────────────────────────────────────────

#[derive(Args)]
struct BackendArgs {
    #[command(subcommand)]
    action: Option<BackendAction>,
}

#[derive(Subcommand)]
enum BackendAction {
    /// Print configured values beside freshly discovered service state
    Inspect,
}

// ── man ───────────────────────────────────────────────────────────────────────

#[derive(Args)]
struct ManArgs {
    /// Show original (untranslated) man output
    #[arg(long)]
    original: bool,
    /// man page name and optional section, e.g. `ls` or `3 printf`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

// ── info ──────────────────────────────────────────────────────────────────────

#[derive(Args)]
struct InfoArgs {
    /// Show original (untranslated) info output
    #[arg(long)]
    original: bool,
    /// info topic and optional options
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

// ── exec ──────────────────────────────────────────────────────────────────────

#[derive(Args)]
struct ExecArgs {
    #[command(subcommand)]
    action: Option<ExecAction>,
    /// Command and arguments to execute (used when no sub-action matches)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    command: Vec<String>,
}

#[derive(Subcommand)]
enum ExecAction {
    /// Install the hymt-exec zsh plugin
    Install(ExecInstallArgs),
    /// Pre-cache translations for recently used commands
    Precache,
}

#[derive(Args)]
struct ExecInstallArgs {
    /// Update the plugin even if it already exists
    #[arg(long)]
    update: bool,
}

// ── tokenizer ─────────────────────────────────────────────────────────────────

#[derive(Args)]
struct TokenizerArgs {
    #[command(subcommand)]
    action: TokenizerAction,
}

#[derive(Subcommand)]
enum TokenizerAction {
    /// Download the tokenizer model
    Download {
        /// Force re-download even if the tokenizer already exists
        #[arg(long)]
        force: bool,
    },
}

// ── estimate ──────────────────────────────────────────────────────────────────

#[derive(Args)]
struct EstimateArgs {
    /// Source character count to estimate
    #[arg(value_name = "SOURCE_CHARS")]
    source_chars: u64,
}

// ── batch ─────────────────────────────────────────────────────────────────────

#[derive(Args)]
struct BatchArgs {
    /// Directory containing files to translate
    directory: PathBuf,
    /// Output directory (mirrors source directory structure)
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Recursively include subdirectories
    #[arg(short = 'r', long)]
    recursive: bool,
    /// Show plan without translating
    #[arg(long)]
    dry_run: bool,
}

// ── history ───────────────────────────────────────────────────────────────────

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
struct HistoryArgs {
    /// Clear all history
    #[arg(long)]
    clear: bool,
    #[command(subcommand)]
    action: Option<HistoryAction>,
}

#[derive(Subcommand)]
enum HistoryAction {
    /// Show performance statistics
    Stats,
    /// Show recent translation entries
    Recent {
        /// Number of entries to show
        #[arg(default_value_t = 10)]
        n: usize,
    },
}

// ── recall ────────────────────────────────────────────────────────────────────

#[derive(Args)]
struct RecallArgs {
    /// Position (1 = most recent)
    #[arg(default_value_t = 1)]
    position: usize,
}

// ── translate-doc ─────────────────────────────────────────────────────────────

#[derive(Args)]
struct TranslateDocArgs {
    /// Source file or directory
    source: PathBuf,
    /// Explicit output path (single file only)
    #[arg(long)]
    output: Option<PathBuf>,
    /// Output directory (mirrors source structure)
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Recursively translate subdirectories
    #[arg(short = 'r', long)]
    recursive: bool,
}

// ── telegram ──────────────────────────────────────────────────────────────────

#[derive(Args)]
struct TelegramArgs {
    /// Regenerate the claim password, print it once, and exit
    #[arg(long)]
    regenerate_claim_password: bool,
}

// ── Smart arg routing ─────────────────────────────────────────────────────────

/// Return the first positional argument from `args` (skip flags and flag values).
fn find_first_positional(args: &[String]) -> Option<&str> {
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            return None;
        }
        if arg.starts_with("--") {
            // Long option: --foo or --foo=val
            if !arg.contains('=') {
                skip_next = true;
            }
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            // Short option cluster; may or may not take value — skip conservatively
            skip_next = true;
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

/// Reorder `args` so that all flags/options come before positional text.
///
/// This mirrors Python's `_option_args_first` and ensures that clap sees
/// `hymt -l zh some text` the same as `hymt some text -l zh`.
fn reorder_for_translation(args: &[String]) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            // Everything after -- is positional
            positional.extend_from_slice(&args[i + 1..]);
            break;
        }
        if arg.starts_with("--") {
            if arg.contains('=') {
                flags.push(arg.clone());
            } else {
                flags.push(arg.clone());
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    i += 1;
                    flags.push(args[i].clone());
                }
            }
        } else if arg.starts_with('-') && arg.len() > 1 {
            flags.push(arg.clone());
            if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                i += 1;
                flags.push(args[i].clone());
            }
        } else {
            positional.push(arg.clone());
        }
        i += 1;
    }
    flags.extend(positional);
    flags
}

/// Parse `--term src=tgt` pairs into `(source, translation)` tuples.
fn parse_terms(terms: &[String]) -> Vec<(String, String)> {
    terms
        .iter()
        .filter_map(|t| {
            t.split_once('=')
                .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        })
        .collect()
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("hymt: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    // Pre-scan raw args to decide whether to reorder before clap parsing.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let first = find_first_positional(&raw);
    let is_known_sub = first
        .map(|s| KNOWN_SUBCOMMANDS.contains(&s))
        .unwrap_or(false);

    let cli = if is_known_sub || first.is_none() {
        // Normal clap parsing — first positional is a subcommand or no args given.
        Cli::parse()
    } else {
        // The first positional is text to translate; reorder so flags come first.
        let mut reordered = vec![std::env::args().next().unwrap_or_else(|| "hymt".to_owned())];
        reordered.extend(reorder_for_translation(&raw));
        Cli::parse_from(reordered)
    };

    let config = HotConfig::new()?;
    eprintln!("{}", profile_startup_diagnostic(config.model_profile()?));
    if config.uses_legacy_generation_scalars() {
        eprintln!("{}", legacy_generation_scalars_migration_warning());
    }
    let generation_settings = config.generation_settings()?;
    if !generation_settings.uses_only_server_defaults() {
        eprintln!("Client sampling overrides: {generation_settings:?}");
    }
    if config.uses_legacy_context_window() {
        eprintln!(
            "Warning: [translation].context_window is deprecated; configure [backend] \
             total_context, parallel_slots, and optional per_request_context instead."
        );
    }
    if should_run_backend_preflight(cli.cmd.as_ref()) {
        let preflight = TranslationClient::new(config.clone())?
            .preflight_backend()
            .await?;
        for warning in preflight.warnings {
            eprintln!("Warning: {warning}");
        }
    } else if matches!(cli.cmd.as_ref(), Some(Cmd::Telegram(_)))
        && should_run_llama_cpp_props_diagnostic(cli.cmd.as_ref())
    {
        if let Some(diagnostic) = TranslationClient::new(config.clone())?
            .llama_cpp_props_diagnostic()
            .await
        {
            eprintln!("{diagnostic}");
        }
    }
    if cli.debug_chunk_timing {
        // CLI flag forces timing logs for this process; config/env also enable them.
        std::env::set_var("HYMT_DEBUG_CHUNK_TIMING", "1");
    }
    let concurrency_override = cli.concurrency;
    let document_policy =
        document_translation_policy(&config, cli.force_translate_all, cli.no_language_detection);
    let default_lang = config.primary_lang();
    let target_lang = cli.lang.as_deref().unwrap_or(&default_lang);
    let explicit_target = cli.lang.is_some();
    let template = TemplateType::from(&cli.template);
    let translate_flags = TranslateFlags {
        show_plan: cli.plan,
        stream_output: should_stream_translation(cli.stream, cli.no_stream, cli.output.as_ref()),
        output_path: cli.output.as_deref(),
        warn_only_completeness: cli.warn_only_completeness,
        concurrency_override,
    };
    let terms = parse_terms(&cli.term);
    let prompt_opts = PromptOpts {
        terms: if terms.is_empty() { None } else { Some(terms) },
        style: cli.style.clone(),
        instructions: cli.instructions.as_deref().map(|s| vec![s.to_owned()]),
        format_type: cli.format_type.clone(),
        context: cli.ctx.clone(),
        document_translation_policy: Some(document_policy),
    };

    match cli.cmd {
        None => {
            // No subcommand: translate from stdin
            run_translate_stdin(
                target_lang,
                &template,
                &prompt_opts,
                explicit_target,
                translate_flags,
                &config,
            )
            .await
        }
        Some(Cmd::Text(words)) if piped_stdin_placeholder(&words, io::stdin().is_terminal()) => {
            run_translate_stdin(
                target_lang,
                &template,
                &prompt_opts,
                explicit_target,
                translate_flags,
                &config,
            )
            .await
        }
        Some(Cmd::Text(words)) => {
            run_translate_text(
                &words,
                target_lang,
                &template,
                &prompt_opts,
                explicit_target,
                translate_flags,
                &config,
            )
            .await
        }
        Some(Cmd::Config(args)) => run_config(args, &config),
        Some(Cmd::Backend(args)) => run_backend(args, &config).await,
        Some(Cmd::Man(args)) => {
            run_man(
                args,
                target_lang,
                explicit_target,
                &config,
                &prompt_opts,
                concurrency_override,
            )
            .await
        }
        Some(Cmd::Info(args)) => {
            run_info(
                args,
                target_lang,
                explicit_target,
                &config,
                &prompt_opts,
                concurrency_override,
            )
            .await
        }
        Some(Cmd::Exec(args)) => {
            run_exec(
                args,
                target_lang,
                explicit_target,
                &config,
                &prompt_opts,
                concurrency_override,
            )
            .await
        }
        Some(Cmd::Tokenizer(args)) => run_tokenizer(args, &config).await,
        Some(Cmd::Estimate(args)) => {
            run_estimate(
                args,
                target_lang,
                &template,
                &prompt_opts,
                &config,
                concurrency_override,
            )
            .await
        }
        Some(Cmd::Batch(args)) => {
            run_batch(
                args,
                target_lang,
                explicit_target,
                &template,
                &prompt_opts,
                cli.plan,
                &config,
                concurrency_override,
            )
            .await
        }
        Some(Cmd::History(args)) => run_history(args),
        Some(Cmd::Recall(args)) => run_recall(args),
        Some(Cmd::TranslateDoc(args)) => {
            run_translate_doc(
                args,
                target_lang,
                explicit_target,
                &template,
                &prompt_opts,
                &config,
                concurrency_override,
            )
            .await
        }
        Some(Cmd::Telegram(args)) => run_telegram(args, &config).await,
    }
}

// ── Shared init helpers ───────────────────────────────────────────────────────

fn make_segmenter(config: &HotConfig) -> Result<Segmenter> {
    let profile = config.model_profile()?;
    hymt_segment::create_segmenter(profile).map_err(|error| anyhow::anyhow!("{error}"))
}

#[cfg(test)]
fn make_segmenter_from_path(tokenizer_path: PathBuf) -> Segmenter {
    if hymt_segment::has_tokenizer_support() && tokenizer_path.exists() {
        Segmenter::new(Some(tokenizer_path)).unwrap_or_else(|_| Segmenter::fallback())
    } else {
        Segmenter::fallback()
    }
}

fn profile_startup_diagnostic(profile: ModelProfile) -> String {
    match profile.tokenizer() {
        Some(source) => format!(
            "Model profile: {} ({}; tokenizer {} @ {})",
            profile.id(),
            profile.architecture().name(),
            source.repo,
            source.revision,
        ),
        None => "Warning: no [endpoint].profile configured; using generic mode without a tested tokenizer or generation defaults.".to_owned(),
    }
}

fn legacy_generation_scalars_migration_warning() -> &'static str {
    "Warning: legacy [inference] sampler scalars are deprecated; move them under [inference.override]."
}

/// Whether this command will make use of the translation service.
///
/// `/props` is an observability preflight, so offline management commands must
/// not pay its timeout or emit authentication diagnostics.
fn should_run_llama_cpp_props_diagnostic(command: Option<&Cmd>) -> bool {
    match command {
        None
        | Some(Cmd::Text(_))
        | Some(Cmd::Man(ManArgs {
            original: false, ..
        }))
        | Some(Cmd::Info(InfoArgs {
            original: false, ..
        }))
        | Some(Cmd::Batch(_))
        | Some(Cmd::TranslateDoc(_))
        | Some(Cmd::Exec(ExecArgs { action: None, .. }))
        | Some(Cmd::Exec(ExecArgs {
            action: Some(ExecAction::Precache),
            ..
        }))
        | Some(Cmd::Telegram(TelegramArgs {
            regenerate_claim_password: false,
        })) => true,
        Some(Cmd::Config(_))
        | Some(Cmd::Backend(_))
        | Some(Cmd::Tokenizer(_))
        | Some(Cmd::Estimate(_))
        | Some(Cmd::History(_))
        | Some(Cmd::Recall(_))
        | Some(Cmd::Man(ManArgs { original: true, .. }))
        | Some(Cmd::Info(InfoArgs { original: true, .. }))
        | Some(Cmd::Exec(ExecArgs {
            action: Some(ExecAction::Install(_)),
            ..
        }))
        | Some(Cmd::Telegram(TelegramArgs {
            regenerate_claim_password: true,
        })) => false,
    }
}

/// Translation paths use the resolved backend state before planning/cache lookup.
/// Keep the pre-existing Telegram probe separate so this feature does not alter
/// bot lifecycle or strict-policy behavior.
fn should_run_backend_preflight(command: Option<&Cmd>) -> bool {
    should_run_llama_cpp_props_diagnostic(command) && !matches!(command, Some(Cmd::Telegram(_)))
}

fn make_client_with_concurrency(
    config: &HotConfig,
    concurrency_override: Option<u32>,
) -> Result<TranslationClient> {
    let concurrency = concurrency_override
        .unwrap_or_else(|| config.concurrency())
        .max(1) as usize;
    TranslationClient::with_concurrency(config.clone(), concurrency)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

async fn run_backend(args: BackendArgs, config: &HotConfig) -> Result<()> {
    match args.action {
        None | Some(BackendAction::Inspect) => {
            let report = TranslationClient::new(config.clone())?
                .inspect_backend()
                .await?;
            print_backend_inspection(&report);
            Ok(())
        }
    }
}

fn inspect_value<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".to_owned())
}

/// Print only configuration and service facts. Deliberately omit endpoint URLs,
/// authorization headers, and API keys from this diagnostic surface.
fn print_backend_inspection(report: &hymt_client::BackendPreflight) {
    let configured = &report.configured;
    let runtime = &report.runtime;
    println!("Backend preflight inspection (credentials omitted)");
    println!("field                         configured                 resolved service");
    println!(
        "backend                       {:<26} {}",
        configured.backend.name(),
        runtime.backend.name()
    );
    println!(
        "model                         {:<26} {}",
        configured.model.as_deref().unwrap_or("unconfigured"),
        runtime.served_model.as_deref().unwrap_or("unavailable")
    );
    println!(
        "profile                       {:<26} {}",
        configured.profile,
        runtime.version.as_deref().unwrap_or("unavailable")
    );
    println!(
        "total context                 {:<26} {}",
        configured.total_context,
        inspect_value(runtime.total_context)
    );
    println!(
        "per-request context           {:<26} {}",
        configured.per_request_context,
        inspect_value(runtime.per_slot_context)
    );
    println!(
        "max output tokens             {:<26} {}",
        configured.max_output_tokens,
        inspect_value(runtime.default_max_generation_tokens)
    );
    println!(
        "active / max slots            {} / {:<21} {} / {}",
        configured.parallel_slots,
        configured.parallel_slots,
        inspect_value(runtime.active_slots),
        inspect_value(runtime.max_parallel_slots)
    );
    println!(
        "sampler overrides             {:?}",
        configured.sampler_overrides
    );
    println!(
        "sampler defaults              {:?}",
        runtime.sampler_defaults
    );
    println!(
        "capabilities                  stream={} tokenize={} template={} structured_output={}",
        inspect_value(runtime.supports_streaming),
        inspect_value(runtime.supports_tokenization),
        inspect_value(runtime.supports_templates),
        inspect_value(runtime.supports_structured_output),
    );
    println!(
        "verification                  {:?}",
        runtime.verification_status
    );
    if report.warnings.is_empty() {
        println!("warnings                      none");
    } else {
        for warning in &report.warnings {
            println!("warning                       {warning}");
        }
    }
}

fn piped_stdin_placeholder(words: &[String], stdin_is_terminal: bool) -> bool {
    !stdin_is_terminal && words.len() == 1 && words[0] == "."
}

// ── Translate text / stdin ────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct TranslateFlags<'a> {
    show_plan: bool,
    stream_output: bool,
    output_path: Option<&'a Path>,
    warn_only_completeness: bool,
    concurrency_override: Option<u32>,
}

fn should_stream_translation(
    stream_enabled: bool,
    no_stream: bool,
    output_path: Option<&PathBuf>,
) -> bool {
    stream_enabled && !no_stream && output_path.is_none()
}

fn completeness_warn_only(config: &HotConfig, flag: bool) -> bool {
    flag || config.completeness_warn_only()
}

fn document_translation_policy(
    config: &HotConfig,
    force_translate_all: bool,
    no_language_detection: bool,
) -> DocumentTranslationPolicy {
    if force_translate_all || no_language_detection {
        DocumentTranslationPolicy::TranslateAll
    } else {
        config.document_translation_policy()
    }
}

fn print_translation_plan(plan: &TranslationPlan) {
    eprintln!(
        "Plan: {} segments, ~{} tokens",
        plan.segments.len(),
        plan.source_tokens
    );
    let budget = &plan.token_budget;
    eprintln!(
        "  token budget: prompt_schema={} source={} profile={} per_slot_context={} input_capacity={} output_reservation={} template_tokens={} safety_margin={} revisions={}",
        budget.prompt_schema,
        budget.counting_source.as_str(),
        budget.profile_id,
        budget.per_slot_context,
        budget.input_capacity_tokens,
        budget.output_reservation_tokens,
        budget.template_tokens,
        budget.safety_margin_tokens,
        budget.revisions,
    );
    eprintln!(
        "  token identity: tokenizer={} chat_template={}",
        budget
            .tokenizer_revision
            .as_deref()
            .unwrap_or("unavailable"),
        budget
            .chat_template_identity
            .as_deref()
            .unwrap_or("unavailable"),
    );
    if !budget.segment_input_tokens.is_empty() {
        eprintln!(
            "  token inputs: min={} max={} available_source_tokens={}",
            budget.segment_input_tokens.iter().min().unwrap_or(&0),
            budget.segment_input_tokens.iter().max().unwrap_or(&0),
            plan.available_source_tokens,
        );
    }
    if let Some(document_plan) = &plan.document_plan {
        for section in document_plan
            .sections
            .iter()
            .filter(|section| section.paragraph_index.is_some())
        {
            eprintln!(
                "  paragraph {}: detected_lang={:?} target_ratio={:?} analyzed_chars={} \
                 is_target_language={} should_translate={}",
                section.paragraph_index.unwrap_or_default() + 1,
                section.detected_lang,
                section.target_ratio,
                section.analyzed_chars,
                section.is_target_language,
                section.should_translate,
            );
        }
    }
}

fn finalize_top_level_outcome(outcome: &TranslationOutcome, warn_only: bool) -> Result<()> {
    if !outcome.is_completeness_degraded() {
        return Ok(());
    }
    outcome.report_completeness_degraded();
    if warn_only {
        return Ok(());
    }
    anyhow::bail!(
        "completeness degraded (best attempt) for segment(s): {}",
        outcome
            .completeness_degraded_segments
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

async fn run_translate_text(
    words: &[String],
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    explicit_target: bool,
    flags: TranslateFlags<'_>,
    config: &HotConfig,
) -> Result<()> {
    // If the single "word" is an existing file path, treat it as translate-file.
    if words.len() == 1 {
        let p = std::path::Path::new(&words[0]);
        if p.exists() && p.is_file() {
            return run_translate_path(
                p,
                target_lang,
                template,
                opts,
                explicit_target,
                flags,
                config,
            )
            .await;
        }
    }

    let text = words.join(" ");
    let segmenter = make_segmenter(config)?;
    let history = HistoryDB::default();
    let effective_lang = if explicit_target {
        target_lang.to_owned()
    } else {
        resolve_target_language(
            &text,
            target_lang,
            &config.primary_lang(),
            &config.secondary_lang(),
            false,
        )
    };

    if flags.show_plan {
        let plan = plan_translation(&text, &effective_lang, config, &segmenter, template, opts)?;
        print_translation_plan(&plan);
        return Ok(());
    }

    let client = make_client_with_concurrency(config, flags.concurrency_override)?;
    let tctx = TranslationCtx {
        config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
    };
    let warn_only = completeness_warn_only(config, flags.warn_only_completeness);
    if flags.stream_output {
        return translate_text_to_stdout_streaming(
            &text,
            &effective_lang,
            template,
            opts,
            &tctx,
            true,
            warn_only,
        )
        .await;
    }
    let outcome = translate_text(&text, &effective_lang, template, opts, &tctx).await?;
    if let Some(out) = flags.output_path {
        write_translation_output(out, &outcome.text).await?;
        return finalize_top_level_outcome(&outcome, warn_only);
    }
    print!("{}", outcome.text);
    if !outcome.text.ends_with('\n') {
        println!();
    }
    finalize_top_level_outcome(&outcome, warn_only)
}

async fn run_translate_stdin(
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    explicit_target: bool,
    flags: TranslateFlags<'_>,
    config: &HotConfig,
) -> Result<()> {
    let stdin_is_terminal = io::stdin().is_terminal();
    let mut text = String::new();
    io::stdin().read_to_string(&mut text)?;
    if text.is_empty() {
        // Print help when called with no args and no stdin
        let mut cmd = Cli::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    }
    // Stdin content is always text; never interpret as a file path.
    let segmenter = make_segmenter(config)?;
    let history = HistoryDB::default();
    let effective_lang = if explicit_target {
        target_lang.to_owned()
    } else {
        resolve_target_language(
            &text,
            target_lang,
            &config.primary_lang(),
            &config.secondary_lang(),
            false,
        )
    };

    if flags.show_plan {
        let plan = plan_translation(&text, &effective_lang, config, &segmenter, template, opts)?;
        print_translation_plan(&plan);
        return Ok(());
    }

    let client = make_client_with_concurrency(config, flags.concurrency_override)?;
    let tctx = TranslationCtx {
        config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
    };
    let warn_only = completeness_warn_only(config, flags.warn_only_completeness);
    if flags.stream_output {
        return translate_text_to_stdout_streaming(
            &text,
            &effective_lang,
            template,
            opts,
            &tctx,
            stdin_is_terminal,
            warn_only,
        )
        .await;
    }
    let outcome = translate_text(&text, &effective_lang, template, opts, &tctx).await?;
    if let Some(out) = flags.output_path {
        write_translation_output(out, &outcome.text).await?;
        return finalize_top_level_outcome(&outcome, warn_only);
    }
    print!("{}", outcome.text);
    if !outcome.text.ends_with('\n') {
        println!();
    }
    finalize_top_level_outcome(&outcome, warn_only)
}

async fn run_translate_path(
    path: &std::path::Path,
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    explicit_target: bool,
    flags: TranslateFlags<'_>,
    config: &HotConfig,
) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let segmenter = make_segmenter(config)?;
    let history = HistoryDB::default();
    let effective_lang = if explicit_target {
        target_lang.to_owned()
    } else {
        resolve_target_language(
            &text,
            target_lang,
            &config.primary_lang(),
            &config.secondary_lang(),
            false,
        )
    };

    if flags.show_plan {
        let plan = plan_translation(&text, &effective_lang, config, &segmenter, template, opts)?;
        print_translation_plan(&plan);
        return Ok(());
    }

    let client = make_client_with_concurrency(config, flags.concurrency_override)?;
    let tctx = TranslationCtx {
        config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
    };
    let warn_only = completeness_warn_only(config, flags.warn_only_completeness);
    if flags.stream_output {
        return translate_text_to_stdout_streaming(
            &text,
            &effective_lang,
            template,
            opts,
            &tctx,
            false,
            warn_only,
        )
        .await;
    }
    let outcome = translate_file(
        path,
        flags.output_path,
        &effective_lang,
        template,
        opts,
        &tctx,
    )
    .await?;
    finalize_top_level_outcome(&outcome, warn_only)
}

async fn translate_text_to_stdout_streaming(
    text: &str,
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    tctx: &TranslationCtx<'_>,
    input_is_interactive: bool,
    warn_only: bool,
) -> Result<()> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let output_mode = current_stream_output_mode(text, input_is_interactive);
    let translate =
        translate_text_stream_with_mode(text, target_lang, template, opts, tctx, output_mode, tx);
    let print = print_stream_events(rx);
    let (outcome, ()) = tokio::try_join!(translate, print)?;
    finalize_top_level_outcome(&outcome, warn_only)
}

fn current_stream_output_mode(input_text: &str, input_is_interactive: bool) -> StreamOutputMode {
    select_stream_output_mode(input_text, input_is_interactive, io::stdout().is_terminal())
}

fn select_stream_output_mode(
    input_text: &str,
    _input_is_interactive: bool,
    _stdout_is_terminal: bool,
) -> StreamOutputMode {
    if looks_like_cli_help_source(input_text) {
        StreamOutputMode::Validated
    } else {
        StreamOutputMode::Optimistic
    }
}

async fn print_stream_events(mut rx: tokio::sync::mpsc::Receiver<StreamEvent>) -> Result<()> {
    let mut stdout = tokio::io::stdout();
    write_stream_events(&mut rx, &mut stdout).await
}

async fn write_stream_events<W>(
    rx: &mut tokio::sync::mpsc::Receiver<StreamEvent>,
    stdout: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut streamed_prefix = String::new();

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::Token(token) => {
                stdout.write_all(token.as_bytes()).await?;
                stdout.flush().await?;
                streamed_prefix.push_str(&token);
            }
            StreamEvent::SegmentDone(_) => {}
            StreamEvent::AllDone(translated) => {
                if let Some(rest) = translated.strip_prefix(&streamed_prefix) {
                    stdout.write_all(rest.as_bytes()).await?;
                } else if streamed_prefix.is_empty() {
                    stdout.write_all(translated.as_bytes()).await?;
                } else {
                    eprintln!(
                        "Warning: streamed prefix differed from final translation; \
                         replaying final translation to avoid truncating stdout"
                    );
                    stdout.write_all(translated.as_bytes()).await?;
                }
                if !translated.ends_with('\n') {
                    stdout.write_all(b"\n").await?;
                }
                stdout.flush().await?;
                break;
            }
        }
    }

    Ok(())
}

// ── config ────────────────────────────────────────────────────────────────────

fn run_config(args: ConfigArgs, config: &HotConfig) -> Result<()> {
    match args.action {
        None | Some(ConfigAction::Show) => {
            let s = config.show()?;
            println!("{s}");
        }
        Some(ConfigAction::Path) => {
            println!("{}", config.path().display());
        }
        Some(ConfigAction::Edit) => {
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_owned());
            std::process::Command::new(&editor)
                .arg(config.path())
                .status()
                .map_err(|e| anyhow::anyhow!("failed to open editor {editor:?}: {e}"))?;
        }
    }
    Ok(())
}

// ── telegram ──────────────────────────────────────────────────────────────────

async fn run_telegram(args: TelegramArgs, config: &HotConfig) -> Result<()> {
    if args.regenerate_claim_password {
        let password = config.regenerate_telegram_claim_password()?;
        // Print once to stdout so the user can store it; not re-printed later.
        println!("{password}");
        eprintln!(
            "hymt telegram: wrote new claim password to {}",
            config.path().display()
        );
        return Ok(());
    }

    #[cfg(feature = "telegram")]
    {
        telegram::run_telegram_bot(config).await
    }
    #[cfg(not(feature = "telegram"))]
    {
        let _ = config;
        anyhow::bail!(
            "hymt was built without the `telegram` cargo feature. \
             Rebuild with default features, or `cargo install --path crates/hymt-cli --features telegram`."
        )
    }
}

// ── man ───────────────────────────────────────────────────────────────────────

async fn run_man(
    args: ManArgs,
    target_lang: &str,
    explicit_target: bool,
    config: &HotConfig,
    opts: &PromptOpts,
    concurrency_override: Option<u32>,
) -> Result<()> {
    if args.args.is_empty() {
        anyhow::bail!("man page name is required");
    }
    let segmenter = make_segmenter(config)?;
    let history = HistoryDB::default();
    let client = make_client_with_concurrency(config, concurrency_override)?;
    let str_args: Vec<&str> = args.args.iter().map(String::as_str).collect();
    let opts = ManInfoOpts {
        target_lang,
        config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        prompt_opts: opts,
        original: args.original,
        explicit_target,
    };
    let code = run_man_command(&str_args, &opts).await?;
    std::process::exit(code);
}

// ── info ──────────────────────────────────────────────────────────────────────

async fn run_info(
    args: InfoArgs,
    target_lang: &str,
    explicit_target: bool,
    config: &HotConfig,
    opts: &PromptOpts,
    concurrency_override: Option<u32>,
) -> Result<()> {
    if args.args.is_empty() {
        anyhow::bail!("info topic is required");
    }
    let segmenter = make_segmenter(config)?;
    let history = HistoryDB::default();
    let client = make_client_with_concurrency(config, concurrency_override)?;
    let str_args: Vec<&str> = args.args.iter().map(String::as_str).collect();
    let opts = ManInfoOpts {
        target_lang,
        config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        prompt_opts: opts,
        original: args.original,
        explicit_target,
    };
    let code = run_info_command(&str_args, &opts).await?;
    std::process::exit(code);
}

// ── exec ──────────────────────────────────────────────────────────────────────

async fn run_exec(
    args: ExecArgs,
    target_lang: &str,
    explicit_target: bool,
    config: &HotConfig,
    opts: &PromptOpts,
    concurrency_override: Option<u32>,
) -> Result<()> {
    match args.action {
        Some(ExecAction::Install(install_args)) => {
            let blocklist = config.exec_plugin_blocklist();
            let result = zsh_plugin::install_zsh_plugin(&blocklist, install_args.update)?;
            eprintln!("Plugin written to: {}", result.plugin_path.display());
            if result.zshrc_updated {
                eprintln!("Added source line to ~/.zshrc");
            } else {
                eprintln!("~/.zshrc already sources the plugin (or was not updated)");
            }
        }
        Some(ExecAction::Precache) => {
            let segmenter = make_segmenter(config)?;
            let history = HistoryDB::default();
            let client = make_client_with_concurrency(config, concurrency_override)?;
            let summary = run_precache(
                target_lang,
                config,
                &client,
                &segmenter,
                &history,
                explicit_target,
                opts,
            )
            .await?;
            eprintln!(
                "Precache complete: {}/{} translated, {} failed",
                summary.translated, summary.total, summary.failed
            );
        }
        None => {
            // Run the command
            let command = if !args.command.is_empty() {
                args.command
            } else {
                anyhow::bail!("exec: command is required");
            };
            // Strip leading -- separator if present
            let command: Vec<&str> = command
                .iter()
                .skip_while(|s| s.as_str() == "--")
                .map(String::as_str)
                .collect();
            if command.is_empty() {
                anyhow::bail!("exec: command is required after --");
            }
            let segmenter = make_segmenter(config)?;
            let history = HistoryDB::default();
            let client = make_client_with_concurrency(config, concurrency_override)?;
            let exec_opts = ExecCommandOpts {
                target_lang,
                config,
                client: &client,
                segmenter: &segmenter,
                history: &history,
                explicit_target,
                prompt_opts: opts,
            };
            let code = run_exec_command(&command, &exec_opts).await?;
            std::process::exit(code);
        }
    }
    Ok(())
}

// ── tokenizer ─────────────────────────────────────────────────────────────────

async fn run_tokenizer(args: TokenizerArgs, config: &HotConfig) -> Result<()> {
    match args.action {
        TokenizerAction::Download { force } => {
            let profile = config.model_profile()?;
            eprintln!("Downloading tokenizer for {}...", profile.id());
            let path = hymt_segment::ensure_tokenizer(profile, force)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            eprintln!("Tokenizer ready at: {}", path.display());
        }
    }
    Ok(())
}

// ── estimate ──────────────────────────────────────────────────────────────────

async fn run_estimate(
    args: EstimateArgs,
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    config: &HotConfig,
    concurrency_override: Option<u32>,
) -> Result<()> {
    let segmenter = make_segmenter(config)?;
    let plan = plan_translation("sample", target_lang, config, &segmenter, template, opts)?;
    let source_lang = estimate_source_lang(target_lang, config);
    let chars_per_segment =
        estimate_chars_per_segment(plan.available_source_tokens, &segmenter, &source_lang);
    let segments = estimate_segment_count(args.source_chars, chars_per_segment)?;

    let history = HistoryDB::default();
    let source_chars = args.source_chars;
    let concurrency = concurrency_override
        .unwrap_or_else(|| config.concurrency())
        .max(1) as i64;
    let template_name = template.as_str();

    eprintln!(
        "Source characters: {source_chars}, estimated segments: {segments}, ~{chars_per_segment} chars/segment, concurrency: {concurrency}, template: {template_name}",
    );

    match history.estimate(
        segments,
        concurrency,
        Some(target_lang),
        Some(template_name),
        Some(config.config_version() as i64),
        None,
    ) {
        Ok(Some(est)) => {
            println!(
                "Estimated time: {:.1}s ({} samples)",
                est.seconds, est.stats.count
            );
        }
        Ok(None) => {
            eprintln!("Not enough history data for an estimate.");
        }
        Err(e) => {
            eprintln!("Estimate failed: {e}");
        }
    }
    Ok(())
}

fn estimate_source_lang(target_lang: &str, config: &HotConfig) -> String {
    let primary = config.primary_lang();
    let secondary = config.secondary_lang();
    if same_language_or_chinese_family(target_lang, &primary) {
        secondary
    } else {
        primary
    }
}

fn same_language_or_chinese_family(left: &str, right: &str) -> bool {
    let (Some(left), Some(right)) = (language_spec_or_none(left), language_spec_or_none(right))
    else {
        return false;
    };
    left.canonical_code == right.canonical_code
        || (left.family == LanguageFamily::Chinese && right.family == LanguageFamily::Chinese)
}

fn estimate_chars_per_segment(
    available_source_tokens: usize,
    segmenter: &Segmenter,
    source_lang: &str,
) -> u64 {
    let sample = build_source_sample(source_lang, 512);
    let sample_chars = sample.chars().count().max(1);
    let sample_tokens = segmenter.count_tokens(&sample).max(1);
    let chars_per_segment =
        (available_source_tokens as u128 * sample_chars as u128) / sample_tokens as u128;
    chars_per_segment.clamp(1, u64::MAX as u128) as u64
}

fn build_source_sample(source_lang: &str, min_chars: usize) -> String {
    let unit = representative_source_text(source_lang);
    let unit_chars = unit.chars().count();
    if min_chars == 0 || unit_chars == 0 {
        return String::new();
    }

    let repeat_count = min_chars.div_ceil(unit_chars);
    let mut sample = String::with_capacity(unit.len() * repeat_count);
    for _ in 0..repeat_count {
        sample.push_str(unit);
    }
    sample
}

fn representative_source_text(source_lang: &str) -> &'static str {
    let Some(spec) = language_spec_or_none(source_lang) else {
        return "This is sample source text used to estimate translation segment size. ";
    };
    if spec.family == LanguageFamily::Chinese {
        return "天地玄黄宇宙洪荒日月盈昃辰宿列张";
    }
    match spec.canonical_code {
        "ja" => "これは日本語の文章です。翻訳の見積もりに使います。",
        "ko" => "이것은 한국어 문장입니다. 번역 추정에 사용합니다.",
        _ => "This is sample source text used to estimate translation segment size. ",
    }
}

fn estimate_segment_count(source_chars: u64, chars_per_segment: u64) -> Result<i64> {
    let segments = source_chars.div_ceil(chars_per_segment.max(1));
    i64::try_from(segments).map_err(|_| anyhow::anyhow!("estimated segment count is too large"))
}

// ── batch ─────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_batch(
    args: BatchArgs,
    target_lang: &str,
    explicit_target: bool,
    template: &TemplateType,
    opts: &PromptOpts,
    show_plan: bool,
    config: &HotConfig,
    concurrency_override: Option<u32>,
) -> Result<()> {
    let segmenter = make_segmenter(config)?;
    let history = HistoryDB::default();
    let plan_opts = BatchPlanOpts {
        output_dir: args.output_dir.as_deref(),
        target_lang,
        template,
        prompt_opts: opts,
        recursive: args.recursive,
        explicit_target,
    };
    let plan = build_batch_plan(&args.directory, config, &segmenter, &history, &plan_opts)?;
    show_batch_preview(&plan);

    if args.dry_run || show_plan {
        return Ok(());
    }

    let client = make_client_with_concurrency(config, concurrency_override)?;
    run_batch_translation(&plan, config, &client, &segmenter, &history, template, opts).await?;
    eprintln!("Batch translation complete: {} files", plan.files.len());
    Ok(())
}

// ── history ───────────────────────────────────────────────────────────────────

fn run_history(args: HistoryArgs) -> Result<()> {
    let history = HistoryDB::default();
    if args.clear {
        let n = history.clear()?;
        eprintln!("Cleared {n} history entries.");
        return Ok(());
    }

    let n_recent = match &args.action {
        Some(HistoryAction::Recent { n }) => *n,
        _ => 10,
    };
    match args.action {
        None | Some(HistoryAction::Recent { .. }) => {
            let previews = history.fetch_recent_translations(n_recent)?;
            if previews.is_empty() {
                eprintln!("No translation history found.");
                return Ok(());
            }
            for p in &previews {
                println!(
                    "[{}] {} ({} → {}) {} chars: {}",
                    p.position,
                    p.finished_at,
                    p.template_type,
                    p.target_lang,
                    p.output_chars,
                    p.preview
                );
            }
        }
        Some(HistoryAction::Stats) => match history.stats(None, None, None)? {
            Some(s) => {
                println!("Translations: {}", s.count);
                println!("Avg tokens/s: {:.1}", s.avg_tokens_per_second);
                println!("Median tokens/s: {:.1}", s.median_tokens_per_second);
                println!("Total input tokens: {}", s.total_input_tokens);
                println!("Total output tokens: {}", s.total_output_tokens);
                println!("Total duration: {:.1}s", s.total_duration_seconds);
            }
            None => eprintln!("No history data."),
        },
    }
    Ok(())
}

// ── recall ────────────────────────────────────────────────────────────────────

fn run_recall(args: RecallArgs) -> Result<()> {
    let history = HistoryDB::default();
    match history.fetch_recent_output(args.position)? {
        Some(text) => {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
        }
        None => {
            eprintln!("No translation at position {}.", args.position);
        }
    }
    Ok(())
}

// ── translate-doc ─────────────────────────────────────────────────────────────

async fn run_translate_doc(
    args: TranslateDocArgs,
    target_lang: &str,
    explicit_target: bool,
    template: &TemplateType,
    opts: &PromptOpts,
    config: &HotConfig,
    concurrency_override: Option<u32>,
) -> Result<()> {
    let segmenter = make_segmenter(config)?;
    let history = HistoryDB::default();
    let client = make_client_with_concurrency(config, concurrency_override)?;
    let doc_opts = DocTranslationOpts {
        target_lang,
        config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        output_path: args.output.as_deref(),
        output_dir: args.output_dir.as_deref(),
        recursive: args.recursive,
        template,
        prompt_opts: opts,
        explicit_target,
    };
    run_doc_translation(&args.source, &doc_opts).await
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    enum CleanupPath {
        File(PathBuf),
        Dir(PathBuf),
    }

    impl Drop for CleanupPath {
        fn drop(&mut self) {
            match self {
                Self::File(path) => {
                    let _ = std::fs::remove_file(path);
                }
                Self::Dir(path) => {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }
    }

    fn unique_test_name(prefix: &str, suffix: &str) -> String {
        format!(
            "{}-{}-{}.{}",
            prefix,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            suffix
        )
    }

    #[test]
    fn find_first_positional_no_args() {
        assert_eq!(find_first_positional(&[]), None);
    }

    #[test]
    fn find_first_positional_only_flags() {
        let args = vec!["--lang".to_owned(), "zh".to_owned(), "--yes".to_owned()];
        assert_eq!(find_first_positional(&args), None);
    }

    #[test]
    fn find_first_positional_subcommand() {
        let args = vec!["man".to_owned(), "ls".to_owned()];
        assert_eq!(find_first_positional(&args), Some("man"));
    }

    #[test]
    fn find_first_positional_flag_then_text() {
        let args = vec!["--lang".to_owned(), "zh".to_owned(), "hello".to_owned()];
        assert_eq!(find_first_positional(&args), Some("hello"));
    }

    #[test]
    fn find_first_positional_after_double_dash() {
        let args = vec!["--".to_owned(), "text".to_owned()];
        assert_eq!(find_first_positional(&args), None);
    }

    #[test]
    fn reorder_flags_before_text() {
        let args = vec!["hello".to_owned(), "--lang".to_owned(), "zh".to_owned()];
        let reordered = reorder_for_translation(&args);
        assert_eq!(
            reordered,
            vec!["--lang".to_owned(), "zh".to_owned(), "hello".to_owned()]
        );
    }

    #[test]
    fn reorder_already_ordered() {
        let args = vec!["--lang".to_owned(), "zh".to_owned(), "hello".to_owned()];
        let reordered = reorder_for_translation(&args);
        assert_eq!(reordered, args);
    }

    #[test]
    fn reorder_only_text() {
        let args = vec!["translate".to_owned(), "this".to_owned()];
        let reordered = reorder_for_translation(&args);
        assert_eq!(reordered, args);
    }

    #[test]
    fn parse_terms_valid() {
        let terms = vec!["foo=bar".to_owned(), "hello=world".to_owned()];
        let parsed = parse_terms(&terms);
        assert_eq!(
            parsed,
            vec![
                ("foo".to_owned(), "bar".to_owned()),
                ("hello".to_owned(), "world".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_terms_skips_invalid() {
        let terms = vec!["noequalssign".to_owned(), "valid=pair".to_owned()];
        let parsed = parse_terms(&terms);
        assert_eq!(parsed, vec![("valid".to_owned(), "pair".to_owned())]);
    }

    #[test]
    fn make_segmenter_falls_back_without_cached_tokenizer() {
        let tokenizer_path = PathBuf::from("target/test-missing-tokenizer/tokenizer.json");
        assert!(!tokenizer_path.exists());

        let segmenter = make_segmenter_from_path(tokenizer_path);
        assert_eq!(segmenter.count_tokens("abcd"), 1);
    }

    #[test]
    fn interactive_terminal_uses_optimistic_streaming() {
        assert_eq!(
            select_stream_output_mode("hello", true, true),
            StreamOutputMode::Optimistic
        );
    }

    #[test]
    fn stdin_pipe_uses_optimistic_streaming_for_normal_text() {
        assert_eq!(
            select_stream_output_mode("hello", false, true),
            StreamOutputMode::Optimistic
        );
    }

    #[test]
    fn stdout_pipe_uses_optimistic_streaming_for_normal_text() {
        assert_eq!(
            select_stream_output_mode("hello", true, false),
            StreamOutputMode::Optimistic
        );
    }

    #[test]
    fn cli_help_uses_validated_streaming() {
        let help = "Usage: verbatim ask [OPTIONS] <QUESTION>...\n\n\
Options:\n  --source-id <SOURCE_ID>\n  --context-only\n";
        assert_eq!(
            select_stream_output_mode(help, false, false),
            StreamOutputMode::Validated
        );
    }

    #[test]
    fn short_option_only_cli_help_uses_validated_streaming() {
        let help = "Usage: foo [OPTIONS]\n\nOptions:\n  -h Print help\n";
        assert_eq!(
            select_stream_output_mode(help, false, false),
            StreamOutputMode::Validated
        );
    }

    #[test]
    fn output_file_disables_default_streaming() {
        let output = PathBuf::from("translated.txt");
        assert!(!should_stream_translation(true, false, Some(&output)));
        assert!(should_stream_translation(true, false, None));
    }

    #[test]
    fn startup_diagnostics_name_the_selected_or_generic_profile() {
        use hymt_core::model_profile::ModelProfile;

        assert!(profile_startup_diagnostic(ModelProfile::HyMt2_30bA3b).contains("hy_mt2_30b_a3b"));
        assert!(profile_startup_diagnostic(ModelProfile::Generic).contains("generic mode"));
    }

    #[test]
    fn legacy_sampler_migration_diagnostic_names_the_override_table() {
        let warning = legacy_generation_scalars_migration_warning();
        assert!(warning.contains("legacy [inference] sampler scalars"));
        assert!(warning.contains("[inference.override]"));
    }

    #[test]
    fn llama_cpp_props_diagnostic_only_preflights_translation_paths() {
        let runs_props_diagnostic = |args: &[&str]| {
            let cli = Cli::try_parse_from(args).expect("parse CLI arguments");
            should_run_llama_cpp_props_diagnostic(cli.cmd.as_ref())
        };

        for args in [
            &["hymt", "text to translate"][..],
            &["hymt", "man", "ls"][..],
            &["hymt", "info", "coreutils"][..],
            &["hymt", "exec", "printf", "hello"][..],
            &["hymt", "exec", "precache"][..],
            &["hymt", "batch", "."][..],
            &["hymt", "translate-doc", "document.md"][..],
            &["hymt", "telegram"][..],
        ] {
            assert!(
                runs_props_diagnostic(args),
                "translation command must run the diagnostic: {args:?}"
            );
        }

        for args in [
            &["hymt", "config", "path"][..],
            &["hymt", "tokenizer", "download"][..],
            &["hymt", "estimate", "1"][..],
            &["hymt", "history"][..],
            &["hymt", "recall"][..],
            &["hymt", "man", "--original", "ls"][..],
            &["hymt", "info", "--original", "coreutils"][..],
            &["hymt", "exec", "install"][..],
            &["hymt", "telegram", "--regenerate-claim-password"][..],
        ] {
            assert!(
                !runs_props_diagnostic(args),
                "offline command must not run the diagnostic: {args:?}"
            );
        }
    }

    #[test]
    fn no_streaming_alias_disables_streaming() {
        let cli = Cli::try_parse_from(["hymt", "--no-streaming", "hello"]).unwrap();
        assert!(cli.no_stream);
        assert!(!should_stream_translation(cli.stream, cli.no_stream, None));
    }

    #[test]
    fn force_translate_all_flag_selects_translate_all_policy() {
        let path = PathBuf::from("target").join(unique_test_name("hymt-cli-force-policy", "toml"));
        let _cleanup = CleanupPath::File(path.clone());
        std::fs::write(&path, "[translation]\n").unwrap();
        let config = HotConfig::from_path(&path).unwrap();

        let cli = Cli::try_parse_from(["hymt", "--force-translate-all", "hello"]).unwrap();
        assert_eq!(
            document_translation_policy(
                &config,
                cli.force_translate_all,
                cli.no_language_detection
            ),
            hymt_core::language::DocumentTranslationPolicy::TranslateAll
        );
    }

    #[test]
    fn no_language_detection_flag_selects_translate_all_policy() {
        let path = PathBuf::from("target").join(unique_test_name("hymt-cli-no-detection", "toml"));
        let _cleanup = CleanupPath::File(path.clone());
        std::fs::write(&path, "[translation]\n").unwrap();
        let config = HotConfig::from_path(&path).unwrap();

        let cli = Cli::try_parse_from(["hymt", "--no-language-detection", "hello"]).unwrap();
        assert_eq!(
            document_translation_policy(
                &config,
                cli.force_translate_all,
                cli.no_language_detection
            ),
            hymt_core::language::DocumentTranslationPolicy::TranslateAll
        );
    }

    #[test]
    fn backend_inspect_is_a_known_offline_command() {
        let cli = Cli::try_parse_from(["hymt", "backend", "inspect"]).unwrap();
        assert!(!should_run_llama_cpp_props_diagnostic(cli.cmd.as_ref()));
        match cli.cmd {
            Some(Cmd::Backend(BackendArgs {
                action: Some(BackendAction::Inspect),
            })) => {}
            _ => panic!("expected backend inspect command"),
        }
    }

    #[tokio::test]
    async fn backend_inspect_handler_runs_fail_open_for_an_unavailable_endpoint() {
        let dir = PathBuf::from("target").join(unique_test_name("hymt-backend-inspect", "dir"));
        let _cleanup = CleanupPath::Dir(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[endpoint]\nurl = \"http://127.0.0.1:1/v1\"\nbackend = \"llama_cpp\"\napi_key = \"inspect-test-secret\"\n\n[translation]\ntimeout = 1\n",
        )
        .unwrap();
        let config = HotConfig::from_path(&path).unwrap();

        run_backend(
            BackendArgs {
                action: Some(BackendAction::Inspect),
            },
            &config,
        )
        .await
        .unwrap();
        assert!(config
            .backend_runtime_info()
            .expect("inspect stores runtime info")
            .verification_message
            .is_some());
    }

    #[test]
    fn concurrency_flag_parses_and_overrides_absent_by_default() {
        let default_cli = Cli::try_parse_from(["hymt", "hello"]).unwrap();
        assert_eq!(default_cli.concurrency, None);
        assert!(!default_cli.debug_chunk_timing);
        assert!(!default_cli.force_translate_all);
        assert!(!default_cli.no_language_detection);

        let cli = Cli::try_parse_from([
            "hymt",
            "--concurrency",
            "4",
            "--debug-chunk-timing",
            "hello",
        ])
        .unwrap();
        assert_eq!(cli.concurrency, Some(4));
        assert!(cli.debug_chunk_timing);
    }

    #[test]
    fn make_client_with_concurrency_override_replaces_config_value() {
        let dir = PathBuf::from("target").join(unique_test_name("hymt-cli-concurrency", "dir"));
        let _cleanup = CleanupPath::Dir(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"[endpoint]
url = "http://127.0.0.1:1/v1"

[translation]
concurrency = 8
timeout = 5
"#,
        )
        .unwrap();
        let cfg = HotConfig::from_path(&path).unwrap();
        assert_eq!(cfg.concurrency(), 8);
        let client = make_client_with_concurrency(&cfg, Some(2)).unwrap();
        assert_eq!(client.concurrency(), 2);
        let default_client = make_client_with_concurrency(&cfg, None).unwrap();
        assert_eq!(default_client.concurrency(), 8);
    }

    #[test]
    fn parses_top_level_output_for_text_translation() {
        let cli = Cli::try_parse_from(["hymt", "--output", "translated.txt", "hello"]).unwrap();
        assert_eq!(
            cli.output.as_deref(),
            Some(std::path::Path::new("translated.txt"))
        );
        match cli.cmd {
            Some(Cmd::Text(words)) => assert_eq!(words, vec!["hello".to_owned()]),
            _ => panic!("expected text command"),
        }
    }

    #[tokio::test]
    async fn writes_single_component_output_path() {
        let output = PathBuf::from(unique_test_name("translated", "txt"));
        let _cleanup = CleanupPath::File(output.clone());
        let _ = std::fs::remove_file(&output);

        write_translation_output(&output, "translated")
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&output).unwrap(), "translated");
    }

    #[tokio::test]
    async fn writes_nested_output_path_creating_parent() {
        let dir = PathBuf::from("target").join(unique_test_name("hymt-cli-output", "dir"));
        let output = dir.join("nested").join("translated.txt");
        let _cleanup = CleanupPath::Dir(dir.clone());
        let _ = std::fs::remove_dir_all(&dir);

        write_translation_output(&output, "translated")
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&output).unwrap(), "translated");
    }

    #[test]
    fn dot_placeholder_uses_piped_stdin() {
        assert!(piped_stdin_placeholder(&[".".to_owned()], false));
    }

    #[test]
    fn dot_placeholder_does_not_override_interactive_text() {
        assert!(!piped_stdin_placeholder(&[".".to_owned()], true));
        assert!(!piped_stdin_placeholder(&["hello".to_owned()], false));
        assert!(!piped_stdin_placeholder(
            &[".".to_owned(), "extra".to_owned()],
            false
        ));
    }

    #[test]
    fn estimate_segment_count_uses_ceiling_division() {
        assert_eq!(estimate_segment_count(0, 2_000).unwrap(), 0);
        assert_eq!(estimate_segment_count(1, 2_000).unwrap(), 1);
        assert_eq!(estimate_segment_count(4_000, 2_000).unwrap(), 2);
        assert_eq!(estimate_segment_count(4_001, 2_000).unwrap(), 3);
    }

    #[test]
    fn estimate_chars_per_segment_depends_on_source_language() {
        let segmenter = Segmenter::fallback();
        let zh_chars = estimate_chars_per_segment(1_500, &segmenter, "zh");
        let en_chars = estimate_chars_per_segment(1_500, &segmenter, "en");

        assert!(zh_chars > 0);
        assert!(en_chars > zh_chars);
    }

    #[test]
    fn estimate_source_lang_matches_language_subtags() {
        let config_path = PathBuf::from("target/test-estimate-source-lang-subtags/config.toml");
        let _ = std::fs::remove_file(&config_path);
        let config = HotConfig::from_path(&config_path).unwrap();

        assert_eq!(estimate_source_lang("zh-cn", &config), "en");
        assert_eq!(estimate_source_lang("zh_cn", &config), "en");
        assert_eq!(estimate_source_lang("ja-jp", &config), "zh");
    }

    #[test]
    fn representative_source_text_matches_language_subtags() {
        assert_eq!(
            representative_source_text("zh-tw"),
            representative_source_text("zh")
        );
        assert_eq!(
            representative_source_text("zh_cn"),
            representative_source_text("zh")
        );
        assert_eq!(
            representative_source_text("ja-jp"),
            representative_source_text("ja")
        );
        assert_eq!(
            representative_source_text("ja_jp"),
            representative_source_text("ja")
        );
        assert_eq!(
            representative_source_text("ko-kr"),
            representative_source_text("ko")
        );
        assert_eq!(
            representative_source_text("ko_kr"),
            representative_source_text("ko")
        );
    }

    #[test]
    fn build_source_sample_reaches_minimum_chars() {
        let sample = build_source_sample("ja-jp", 513);

        assert!(sample.chars().count() >= 513);
    }

    #[test]
    fn parses_history_clear_flag() {
        let cli = Cli::try_parse_from(["hymt", "history", "--clear"]).unwrap();
        match cli.cmd {
            Some(Cmd::History(args)) => {
                assert!(args.clear);
                assert!(args.action.is_none());
            }
            _ => panic!("expected history command"),
        }
    }

    #[test]
    fn rejects_history_clear_with_subcommand() {
        let err = match Cli::try_parse_from(["hymt", "history", "--clear", "stats"]) {
            Ok(_) => panic!("expected argument conflict"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parses_estimate_source_character_count() {
        let cli = Cli::try_parse_from(["hymt", "estimate", "10000"]).unwrap();
        match cli.cmd {
            Some(Cmd::Estimate(args)) => {
                assert_eq!(args.source_chars, 10_000);
            }
            _ => panic!("expected estimate command"),
        }
    }

    #[test]
    fn rejects_estimate_non_integer() {
        let err = match Cli::try_parse_from(["hymt", "estimate", "README.md"]) {
            Ok(_) => panic!("expected parse error"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn known_subcommands_routing() {
        for sub in KNOWN_SUBCOMMANDS {
            let args = vec![sub.to_string()];
            let first = find_first_positional(&args);
            assert!(
                first
                    .map(|s| KNOWN_SUBCOMMANDS.contains(&s))
                    .unwrap_or(false)
                    || *sub == "--help"
                    || *sub == "-h"
                    || *sub == "--version"
                    || *sub == "-V"
            );
        }
    }
}
