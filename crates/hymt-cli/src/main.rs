mod zsh_plugin;

use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, CommandFactory, Parser, Subcommand};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use hymt_cache::history::HistoryDB;
use hymt_client::TranslationClient;
use hymt_core::config::HotConfig;
use hymt_core::language::resolve_target_language;
use hymt_core::templates::{PromptOpts, TemplateType};
use hymt_segment::Segmenter;
use hymt_translate::batch::{
    build_batch_plan, run_batch_translation, show_batch_preview, BatchPlanOpts,
};
use hymt_translate::doc_translate::{run_doc_translation, DocTranslationOpts};
use hymt_translate::docs::{run_info_command, run_man_command, ManInfoOpts};
use hymt_translate::exec_wrapper::run_exec_command;
use hymt_translate::precache::run_precache;
use hymt_translate::{
    plan_translation, translate_file, translate_text, translate_text_stream_with_mode, StreamEvent,
    StreamOutputMode, TranslationCtx,
};

// ── Known subcommand names (for smart routing) ────────────────────────────────

const KNOWN_SUBCOMMANDS: &[&str] = &[
    "config",
    "man",
    "info",
    "exec",
    "tokenizer",
    "estimate",
    "batch",
    "history",
    "recall",
    "translate-doc",
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

    /// Enable streaming output
    #[arg(long, global = true, overrides_with = "no_stream")]
    stream: bool,

    /// Disable streaming
    #[arg(long = "no-stream", global = true)]
    no_stream: bool,

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
    /// Watch source for changes and re-translate (reserved; not yet implemented)
    #[arg(long)]
    watch: bool,
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
    let default_lang = config.primary_lang();
    let target_lang = cli.lang.as_deref().unwrap_or(&default_lang);
    let explicit_target = cli.lang.is_some();
    let template = TemplateType::from(&cli.template);
    let translate_flags = TranslateFlags {
        show_plan: cli.plan,
        stream_output: cli.stream && !cli.no_stream,
    };
    let terms = parse_terms(&cli.term);
    let prompt_opts = PromptOpts {
        terms: if terms.is_empty() { None } else { Some(terms) },
        style: cli.style.clone(),
        instructions: cli.instructions.as_deref().map(|s| vec![s.to_owned()]),
        format_type: cli.format_type.clone(),
        context: cli.ctx.clone(),
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
        Some(Cmd::Man(args)) => {
            run_man(args, target_lang, explicit_target, &config, &prompt_opts).await
        }
        Some(Cmd::Info(args)) => {
            run_info(args, target_lang, explicit_target, &config, &prompt_opts).await
        }
        Some(Cmd::Exec(args)) => {
            run_exec(args, target_lang, explicit_target, &config, &prompt_opts).await
        }
        Some(Cmd::Tokenizer(args)) => run_tokenizer(args).await,
        Some(Cmd::Estimate(args)) => {
            run_estimate(args, target_lang, &template, &prompt_opts, &config).await
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
            )
            .await
        }
    }
}

// ── Shared init helpers ───────────────────────────────────────────────────────

fn make_segmenter() -> Segmenter {
    make_segmenter_from_path(hymt_segment::tokenizer_path())
}

fn make_segmenter_from_path(tokenizer_path: PathBuf) -> Segmenter {
    if hymt_segment::has_tokenizer_support() && tokenizer_path.exists() {
        Segmenter::new(Some(tokenizer_path)).unwrap_or_else(|_| Segmenter::fallback())
    } else {
        Segmenter::fallback()
    }
}

fn make_client(config: &HotConfig) -> Result<TranslationClient> {
    TranslationClient::new(config.clone()).map_err(|e| anyhow::anyhow!("{e}"))
}

// ── Translate text / stdin ────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct TranslateFlags {
    show_plan: bool,
    stream_output: bool,
}

async fn run_translate_text(
    words: &[String],
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    explicit_target: bool,
    flags: TranslateFlags,
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
    let segmenter = make_segmenter();
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
        eprintln!(
            "Plan: {} segments, ~{} tokens",
            plan.segments.len(),
            plan.source_tokens
        );
        return Ok(());
    }

    let client = make_client(config)?;
    let tctx = TranslationCtx {
        config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
    };
    if flags.stream_output {
        return translate_text_to_stdout_streaming(&text, &effective_lang, template, opts, &tctx)
            .await;
    }
    let translated = translate_text(&text, &effective_lang, template, opts, &tctx).await?;
    print!("{translated}");
    if !translated.ends_with('\n') {
        println!();
    }
    Ok(())
}

async fn run_translate_stdin(
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    explicit_target: bool,
    flags: TranslateFlags,
    config: &HotConfig,
) -> Result<()> {
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
    let segmenter = make_segmenter();
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
        eprintln!(
            "Plan: {} segments, ~{} tokens",
            plan.segments.len(),
            plan.source_tokens
        );
        return Ok(());
    }

    let client = make_client(config)?;
    let tctx = TranslationCtx {
        config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
    };
    if flags.stream_output {
        return translate_text_to_stdout_streaming(&text, &effective_lang, template, opts, &tctx)
            .await;
    }
    let translated = translate_text(&text, &effective_lang, template, opts, &tctx).await?;
    print!("{translated}");
    if !translated.ends_with('\n') {
        println!();
    }
    Ok(())
}

async fn run_translate_path(
    path: &std::path::Path,
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    explicit_target: bool,
    flags: TranslateFlags,
    config: &HotConfig,
) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let segmenter = make_segmenter();
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
        eprintln!(
            "Plan: {} segments, ~{} tokens",
            plan.segments.len(),
            plan.source_tokens
        );
        return Ok(());
    }

    let client = make_client(config)?;
    let tctx = TranslationCtx {
        config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
    };
    if flags.stream_output {
        return translate_text_to_stdout_streaming(&text, &effective_lang, template, opts, &tctx)
            .await;
    }
    translate_file(path, None, &effective_lang, template, opts, &tctx).await?;
    Ok(())
}

async fn translate_text_to_stdout_streaming(
    text: &str,
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    tctx: &TranslationCtx<'_>,
) -> Result<()> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let output_mode = if io::stdout().is_terminal() {
        StreamOutputMode::Optimistic
    } else {
        StreamOutputMode::Validated
    };
    let translate =
        translate_text_stream_with_mode(text, target_lang, template, opts, tctx, output_mode, tx);
    let print = print_stream_events(rx);
    let (_translated, ()) = tokio::try_join!(translate, print)?;
    Ok(())
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

// ── man ───────────────────────────────────────────────────────────────────────

async fn run_man(
    args: ManArgs,
    target_lang: &str,
    explicit_target: bool,
    config: &HotConfig,
    _opts: &PromptOpts,
) -> Result<()> {
    if args.args.is_empty() {
        anyhow::bail!("man page name is required");
    }
    let segmenter = make_segmenter();
    let history = HistoryDB::default();
    let client = make_client(config)?;
    let str_args: Vec<&str> = args.args.iter().map(String::as_str).collect();
    let opts = ManInfoOpts {
        target_lang,
        config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
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
    _opts: &PromptOpts,
) -> Result<()> {
    if args.args.is_empty() {
        anyhow::bail!("info topic is required");
    }
    let segmenter = make_segmenter();
    let history = HistoryDB::default();
    let client = make_client(config)?;
    let str_args: Vec<&str> = args.args.iter().map(String::as_str).collect();
    let opts = ManInfoOpts {
        target_lang,
        config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
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
    _opts: &PromptOpts,
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
            let segmenter = make_segmenter();
            let history = HistoryDB::default();
            let client = make_client(config)?;
            let summary = run_precache(
                target_lang,
                config,
                &client,
                &segmenter,
                &history,
                explicit_target,
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
            let segmenter = make_segmenter();
            let history = HistoryDB::default();
            let client = make_client(config)?;
            let code = run_exec_command(
                &command,
                target_lang,
                config,
                &client,
                &segmenter,
                &history,
                explicit_target,
            )
            .await?;
            std::process::exit(code);
        }
    }
    Ok(())
}

// ── tokenizer ─────────────────────────────────────────────────────────────────

async fn run_tokenizer(args: TokenizerArgs) -> Result<()> {
    match args.action {
        TokenizerAction::Download { force } => {
            eprintln!("Downloading tokenizer...");
            let path = hymt_segment::ensure_tokenizer(force).map_err(|e| anyhow::anyhow!("{e}"))?;
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
) -> Result<()> {
    let segmenter = make_segmenter();
    let plan = plan_translation("sample", target_lang, config, &segmenter, template, opts)?;
    let source_lang = estimate_source_lang(target_lang, config);
    let chars_per_segment =
        estimate_chars_per_segment(plan.available_source_tokens, &segmenter, &source_lang);
    let segments = estimate_segment_count(args.source_chars, chars_per_segment)?;

    let history = HistoryDB::default();
    let source_chars = args.source_chars;
    let concurrency = config.concurrency() as i64;
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
    let norm_target = normalize_lang(target_lang);
    let norm_primary = normalize_lang(&primary);
    if base_lang(&norm_target) == base_lang(&norm_primary) {
        secondary
    } else {
        primary
    }
}

fn normalize_lang(lang: &str) -> String {
    lang.trim().to_ascii_lowercase()
}

fn base_lang(lang: &str) -> &str {
    lang.split('-').next().unwrap_or("")
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
    let norm = normalize_lang(source_lang);
    match base_lang(&norm) {
        "zh" => "天地玄黄宇宙洪荒日月盈昃辰宿列张",
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

async fn run_batch(
    args: BatchArgs,
    target_lang: &str,
    explicit_target: bool,
    template: &TemplateType,
    opts: &PromptOpts,
    show_plan: bool,
    config: &HotConfig,
) -> Result<()> {
    let segmenter = make_segmenter();
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

    let client = make_client(config)?;
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
) -> Result<()> {
    let segmenter = make_segmenter();
    let history = HistoryDB::default();
    let client = make_client(config)?;
    let doc_opts = DocTranslationOpts {
        target_lang,
        config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        output_path: args.output.as_deref(),
        output_dir: args.output_dir.as_deref(),
        recursive: args.recursive,
        watch: args.watch,
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
        assert_eq!(estimate_source_lang("ja-jp", &config), "zh");
    }

    #[test]
    fn representative_source_text_matches_language_subtags() {
        assert_eq!(
            representative_source_text("zh-tw"),
            representative_source_text("zh")
        );
        assert_eq!(
            representative_source_text("ja-jp"),
            representative_source_text("ja")
        );
        assert_eq!(
            representative_source_text("ko-kr"),
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
