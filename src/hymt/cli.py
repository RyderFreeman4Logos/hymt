from __future__ import annotations

from pathlib import Path
import asyncio
import os
import shlex
import subprocess
import sys

import click

from hymt.batch import build_batch_plan, run_batch_translation, show_batch_preview
from hymt.config import HotConfig, config_path, show
from hymt.docs import show_translated_info, show_translated_man
from hymt.exec_wrapper import run_exec_command
from hymt.history import (
    HistoryDB,
    TaskRecord,
    TranslationPreview,
    estimate_duration_seconds,
    format_duration,
)
from hymt.language import DocumentLanguagePlan, analyze_document_language
from hymt.precache import run_precache
from hymt.segment import TOKENIZER_PATH, ensure_tokenizer, has_tokenizer_support
from hymt.templates import TemplateType
from hymt.translate import plan_translation, translate_file, translate_text
from hymt.zsh_plugin import install_zsh_plugin


TEMPLATE_CHOICES = [template.value for template in TemplateType]
VALUE_OPTIONS = frozenset(
    {
        "--target",
        "-t",
        "--file",
        "-f",
        "--output",
        "-o",
        "--type",
        "--terms",
        "--style",
        "--context",
        "--format",
        "--instruction",
    }
)
LONG_VALUE_PREFIXES = tuple(
    f"{option}=" for option in VALUE_OPTIONS if option.startswith("--")
)
SHORT_VALUE_OPTIONS = frozenset({"-t", "-f", "-o"})


class TranslationGroup(click.Group):
    def parse_args(self, ctx: click.Context, args: list[str]) -> list[str]:
        if not args and self.no_args_is_help and not ctx.resilient_parsing:
            raise click.NoArgsIsHelpError(ctx)
        first_command = self._first_command_candidate(ctx, args)
        treat_rest_as_text = first_command is None and self._has_argument_separator(
            args
        )
        if first_command is None:
            args = self._option_args_first(args)
        rest = click.Command.parse_args(self, ctx, args)
        if rest:
            if treat_rest_as_text:
                ctx._protected_args, ctx.args = [], rest
            else:
                cmd_name = rest[0]
                command = self.get_command(ctx, cmd_name)
                if command is not None:
                    ctx._protected_args, ctx.args = rest[:1], rest[1:]
                else:
                    ctx._protected_args, ctx.args = [], rest
        return ctx.args

    def _first_command_candidate(
        self, ctx: click.Context, args: list[str]
    ) -> str | None:
        index = 0
        while index < len(args):
            arg = args[index]
            if arg == "--":
                return None
            if arg in VALUE_OPTIONS:
                index += 2
                continue
            if arg.startswith(LONG_VALUE_PREFIXES):
                index += 1
                continue
            if _is_attached_short_value(arg):
                index += 1
                continue
            if arg.startswith("-"):
                index += 1
                continue
            return arg if self.get_command(ctx, arg) is not None else None
        return None

    def _option_args_first(self, args: list[str]) -> list[str]:
        option_args: list[str] = []
        text_args: list[str] = []
        index = 0
        after_separator = False
        while index < len(args):
            arg = args[index]
            if after_separator:
                text_args.append(arg)
            elif arg == "--":
                after_separator = True
            elif arg in VALUE_OPTIONS:
                option_args.append(arg)
                if index + 1 < len(args):
                    option_args.append(args[index + 1])
                    index += 1
            elif arg.startswith(LONG_VALUE_PREFIXES) or _is_attached_short_value(arg):
                option_args.append(arg)
            elif arg.startswith("-"):
                option_args.append(arg)
            else:
                text_args.append(arg)
            index += 1
        return [*option_args, *text_args]

    def _has_argument_separator(self, args: list[str]) -> bool:
        index = 0
        while index < len(args):
            arg = args[index]
            if arg == "--":
                return True
            if arg in VALUE_OPTIONS:
                index += 2
                continue
            if arg.startswith(LONG_VALUE_PREFIXES) or _is_attached_short_value(arg):
                index += 1
                continue
            index += 1
        return False


def _is_attached_short_value(arg: str) -> bool:
    return any(
        arg.startswith(option) and arg != option for option in SHORT_VALUE_OPTIONS
    )


@click.group(
    cls=TranslationGroup,
    invoke_without_command=True,
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True},
    help="Translate positional text, a file, or stdin.",
)
@click.option(
    "--target",
    "-t",
    "target_lang",
    default="zh",
    show_default=True,
    help="Target language code.",
)
@click.option(
    "--file",
    "-f",
    "input_file",
    type=click.Path(path_type=Path),
    help="Input file path.",
)
@click.option(
    "--output",
    "-o",
    "output_file",
    type=click.Path(path_type=Path),
    help="Output file path.",
)
@click.option(
    "--type",
    "template_type",
    type=click.Choice(TEMPLATE_CHOICES),
    default=TemplateType.DEFAULT.value,
    show_default=True,
    help="Template type.",
)
@click.option("--terms", multiple=True, help="Terminology pair, format: source=target.")
@click.option("--style", help="Style description for style translations.")
@click.option(
    "--context",
    "background_context",
    help="Background context for context-aware translations.",
)
@click.option(
    "--format", "format_type", help="Data format for structured translations."
)
@click.option(
    "--instruction", "instructions", multiple=True, help="Personalization instruction."
)
@click.option(
    "--yes",
    is_flag=True,
    help="Skip interactive confirmation prompts.",
)
@click.option(
    "--stream/--no-stream",
    "stream",
    default=None,
    help="Override [translation].stream for stdout streaming.",
)
@click.pass_context
def main(
    ctx: click.Context,
    target_lang: str | None,
    input_file: Path | None,
    output_file: Path | None,
    template_type: str,
    terms: tuple[str, ...],
    style: str | None,
    background_context: str | None,
    format_type: str | None,
    instructions: tuple[str, ...],
    yes: bool,
    stream: bool | None,
) -> None:
    if ctx.invoked_subcommand is not None:
        return
    text = " ".join(ctx.args) if ctx.args else None
    assert target_lang is not None
    if input_file is not None and text is not None:
        raise click.UsageError("Use either positional text or --file, not both")

    try:
        kwargs = _template_kwargs(
            terms, style, background_context, format_type, instructions
        )
        config = HotConfig()
        stream_enabled = _stream_enabled(config, stream)
        selected_type = TemplateType(template_type)
        if input_file is not None:
            source_text = input_file.read_text(encoding="utf-8")
            document_plan = _select_document_translation_plan(
                source_text, target_lang, yes
            )
            if document_plan is None:
                click.echo("Translation cancelled.", err=True)
                return
            _announce_tokenizer_download()
            asyncio.run(
                translate_file(
                    input_file,
                    output_file,
                    target_lang,
                    config,
                    selected_type,
                    stream=stream_enabled,
                    source_text=source_text,
                    document_plan=document_plan,
                    **kwargs,
                )
            )
            return
        source_text = text if text is not None else sys.stdin.read()
        document_plan = _select_document_translation_plan(source_text, target_lang, yes)
        if document_plan is None:
            click.echo("Translation cancelled.", err=True)
            return
        _announce_tokenizer_download()
        streamed_chars = 0

        def write_token(token: str) -> None:
            nonlocal streamed_chars
            streamed_chars += len(token)
            sys.stdout.write(token)
            sys.stdout.flush()

        translated = asyncio.run(
            translate_text(
                source_text,
                target_lang,
                config,
                selected_type,
                stream=stream_enabled,
                on_token=write_token
                if output_file is None and stream_enabled
                else None,
                document_plan=document_plan,
                **kwargs,
            )
        )
        if output_file is None:
            if not stream_enabled or streamed_chars == 0:
                sys.stdout.write(translated)
            if not translated.endswith("\n"):
                sys.stdout.write("\n")
            sys.stdout.flush()
            return
        output_file.parent.mkdir(parents=True, exist_ok=True)
        output_file.write_text(translated, encoding="utf-8")
    except (OSError, ValueError, RuntimeError) as exc:
        raise click.ClickException(str(exc)) from exc


@main.group()
def config() -> None:
    pass


@config.command("show")
def config_show() -> None:
    try:
        click.echo(show(), nl=False)
    except OSError as exc:
        raise click.ClickException(str(exc)) from exc


@config.command("path")
def config_path_command() -> None:
    click.echo(str(config_path()))


@config.command("edit")
def config_edit() -> None:
    try:
        path = HotConfig().path
    except OSError as exc:
        raise click.ClickException(str(exc)) from exc
    editor = os.environ.get("EDITOR")
    if not editor:
        raise click.ClickException("EDITOR is not set")
    command = [*shlex.split(editor), str(path)]
    result = subprocess.run(command, check=False)
    if result.returncode != 0:
        raise click.ClickException(f"Editor exited with status {result.returncode}")


@main.command(
    "man",
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True},
)
@click.option(
    "--target",
    "-t",
    "target_lang",
    default="zh",
    show_default=True,
    help="Target language code.",
)
@click.option("--original", is_flag=True, help="Show the untranslated system manpage.")
@click.option("--refresh", is_flag=True, help="Force a fresh translation.")
@click.argument("man_args", nargs=-1, type=click.UNPROCESSED)
def man_command(
    target_lang: str, original: bool, refresh: bool, man_args: tuple[str, ...]
) -> None:
    try:
        returncode = show_translated_man(
            man_args,
            target_lang,
            HotConfig(),
            original=original,
            refresh=refresh,
        )
    except (OSError, ValueError, RuntimeError) as exc:
        raise click.ClickException(str(exc)) from exc
    raise click.exceptions.Exit(returncode)


@main.command(
    "info",
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True},
)
@click.option(
    "--target",
    "-t",
    "target_lang",
    default="zh",
    show_default=True,
    help="Target language code.",
)
@click.option("--original", is_flag=True, help="Show the untranslated info page.")
@click.option("--refresh", is_flag=True, help="Force a fresh translation.")
@click.argument("info_args", nargs=-1, type=click.UNPROCESSED)
def info_command(
    target_lang: str, original: bool, refresh: bool, info_args: tuple[str, ...]
) -> None:
    try:
        returncode = show_translated_info(
            info_args,
            target_lang,
            HotConfig(),
            original=original,
            refresh=refresh,
        )
    except (OSError, ValueError, RuntimeError) as exc:
        raise click.ClickException(str(exc)) from exc
    raise click.exceptions.Exit(returncode)


@main.group(
    "exec",
    invoke_without_command=True,
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True},
    help="Run a command and translate its output after completion.",
)
@click.option(
    "--target",
    "-t",
    "target_lang",
    default="zh",
    show_default=True,
    help="Target language code.",
)
@click.pass_context
def exec_command(ctx: click.Context, target_lang: str) -> None:
    if ctx.invoked_subcommand is not None:
        return
    command = list(ctx.args)
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        raise click.UsageError("Use 'hymt exec -- command args...'")
    try:
        returncode = run_exec_command(command, target_lang, HotConfig())
    except (OSError, ValueError, RuntimeError) as exc:
        raise click.ClickException(str(exc)) from exc
    raise click.exceptions.Exit(returncode)


@exec_command.command("install")
@click.option("--update", is_flag=True, help="Overwrite an existing zsh plugin.")
def exec_install_command(update: bool) -> None:
    try:
        result = install_zsh_plugin(HotConfig(), update=update)
    except (OSError, ValueError) as exc:
        raise click.ClickException(str(exc)) from exc
    click.echo(f"Installed {result.plugin_path}")
    if result.updated_zshrc:
        click.echo(f"Added source line to {result.zshrc_path}")
    else:
        click.echo(f"Source line already present in {result.zshrc_path}")


@exec_command.command("precache")
@click.option(
    "--target",
    "-t",
    "target_lang",
    default="zh",
    show_default=True,
    help="Target language code.",
)
@click.option(
    "--recursive",
    is_flag=True,
    help="Also translate discovered subcommand --help output.",
)
@click.option("--section", help="Only translate a specific man section.")
def exec_precache_command(
    target_lang: str, recursive: bool, section: str | None
) -> None:
    try:
        summary = run_precache(
            target_lang,
            HotConfig(),
            recursive=recursive,
            section=section,
            progress_stream=sys.stderr,
        )
    except (OSError, ValueError, RuntimeError) as exc:
        raise click.ClickException(str(exc)) from exc
    click.echo(
        "Precache complete: "
        f"{summary.translated}/{summary.total} translated, {summary.failed} failed.",
        err=True,
    )


@main.group()
def tokenizer() -> None:
    pass


@tokenizer.command("download")
@click.option("--force", is_flag=True, help="Force a fresh tokenizer download.")
def tokenizer_download(force: bool) -> None:
    if not has_tokenizer_support():
        raise click.ClickException(
            "The tokenizer dependency is not installed on this platform; "
            "hymt will use approximate token counting."
        )
    click.echo("Downloading tokenizer...", err=True)
    try:
        path = ensure_tokenizer(force_download=force)
    except OSError as exc:
        raise click.ClickException(str(exc)) from exc
    click.echo(path)


@main.command("estimate")
@click.option(
    "--file",
    "-f",
    "input_file",
    type=click.Path(path_type=Path),
    help="Input file path.",
)
@click.option(
    "--target", "-t", "target_lang", required=True, help="Target language code."
)
@click.option(
    "--type",
    "template_type",
    type=click.Choice(TEMPLATE_CHOICES),
    default=TemplateType.DEFAULT.value,
    show_default=True,
    help="Template type.",
)
@click.option("--terms", multiple=True, help="Terminology pair, format: source=target.")
@click.option("--style", help="Style description for style translations.")
@click.option(
    "--context",
    "background_context",
    help="Background context for context-aware translations.",
)
@click.option(
    "--format", "format_type", help="Data format for structured translations."
)
@click.option(
    "--instruction", "instructions", multiple=True, help="Personalization instruction."
)
def estimate_command(
    input_file: Path | None,
    target_lang: str,
    template_type: str,
    terms: tuple[str, ...],
    style: str | None,
    background_context: str | None,
    format_type: str | None,
    instructions: tuple[str, ...],
) -> None:
    try:
        text = (
            input_file.read_text(encoding="utf-8")
            if input_file is not None
            else sys.stdin.read()
        )
        config = HotConfig()
        selected_type = TemplateType(template_type)
        _announce_tokenizer_download()
        plan = plan_translation(
            text,
            target_lang,
            config,
            selected_type,
            terms=terms,
            style=style,
            background_text=background_context,
            format_type=format_type,
            instructions=instructions,
        )
    except (OSError, ValueError, RuntimeError) as exc:
        raise click.ClickException(str(exc)) from exc

    click.echo(f"Input: {len(text):,} chars (~{plan.source_tokens:,} tokens)")
    click.echo(f"Estimated segments: {plan.segment_count}")
    estimate = HistoryDB().estimate(
        plan.segment_count,
        config.concurrency,
        target_lang,
        selected_type.value,
    )
    if estimate is None:
        click.echo("No historical data yet - run some translations first.")
        return

    stats = estimate.stats
    click.echo(
        f"Based on {stats.count} historical tasks "
        f"(avg {stats.avg_tokens_per_second:.1f} tok/s, "
        f"p50 {stats.median_tokens_per_second:.1f}, "
        f"p5 {stats.p5_tokens_per_second:.1f}, "
        f"p95 {stats.p95_tokens_per_second:.1f}):"
    )
    click.echo(
        "  Estimated time: "
        f"~{format_duration(estimate_duration_seconds(stats, plan.segment_count, 1))} "
        "(concurrency=1)"
    )
    if config.concurrency != 1:
        click.echo(
            "  Estimated time: "
            f"~{format_duration(estimate.seconds)} "
            f"(concurrency={config.concurrency})"
        )


@main.command("batch")
@click.argument(
    "directory",
    required=False,
    default=".",
    type=click.Path(path_type=Path, file_okay=False, dir_okay=True),
)
@click.option("--target", "-t", "target_lang", default="zh", show_default=True)
@click.option(
    "--output-dir",
    type=click.Path(path_type=Path, file_okay=False, dir_okay=True),
    help="Directory for translated files; preserves source-relative paths.",
)
@click.option("--write", is_flag=True, help="Write translated files.")
@click.option("--yes", is_flag=True, help="Skip batch confirmation.")
@click.option(
    "--type",
    "template_type",
    type=click.Choice(TEMPLATE_CHOICES),
    default=TemplateType.DEFAULT.value,
    show_default=True,
    help="Template type.",
)
@click.option("--terms", multiple=True, help="Terminology pair, format: source=target.")
@click.option("--style", help="Style description for style translations.")
@click.option(
    "--context",
    "background_context",
    help="Background context for context-aware translations.",
)
@click.option(
    "--format", "format_type", help="Data format for structured translations."
)
@click.option(
    "--instruction", "instructions", multiple=True, help="Personalization instruction."
)
@click.option(
    "--stream/--no-stream",
    "stream",
    default=None,
    help="Override [translation].stream for batch translation.",
)
def batch_command(
    directory: Path,
    target_lang: str,
    output_dir: Path | None,
    write: bool,
    yes: bool,
    template_type: str,
    terms: tuple[str, ...],
    style: str | None,
    background_context: str | None,
    format_type: str | None,
    instructions: tuple[str, ...],
    stream: bool | None,
) -> None:
    try:
        kwargs = _template_kwargs(
            terms, style, background_context, format_type, instructions
        )
        config = HotConfig()
        selected_type = TemplateType(template_type)
        _announce_tokenizer_download()
        plan = build_batch_plan(
            directory,
            output_dir,
            target_lang,
            config,
            selected_type,
            kwargs,
        )
        show_batch_preview(plan, sys.stderr)
        if not write:
            click.echo(
                "Dry run: no files written. Re-run with --write to translate.", err=True
            )
            return
        if not plan.files:
            click.echo("No files to translate.", err=True)
            return
        if not yes and not click.confirm(
            f"Translate and write {len(plan.files)} files?", default=False, err=True
        ):
            click.echo("Batch translation cancelled.", err=True)
            return
        asyncio.run(
            run_batch_translation(
                plan,
                target_lang,
                config,
                selected_type,
                stream=stream,
                template_kwargs=kwargs,
            )
        )
    except (OSError, UnicodeError, ValueError, RuntimeError) as exc:
        raise click.ClickException(str(exc)) from exc


@main.command("history")
@click.option("--all", "show_all", is_flag=True, help="Show all history records.")
@click.option("--stats", "show_stats", is_flag=True, help="Show aggregate statistics.")
@click.option("--clear", is_flag=True, help="Clear all history records.")
def history_command(show_all: bool, show_stats: bool, clear: bool) -> None:
    db = HistoryDB()
    if clear:
        if not click.confirm("Clear all history records?", default=False, err=True):
            click.echo("History not cleared.")
            return
        deleted = db.clear()
        click.echo(f"Cleared {deleted} history records.")
        return

    if show_stats:
        _show_history_stats(db)
        return

    records = db.fetch_recent(limit=None if show_all else 10)
    _show_history_records(records)


@main.command("recall")
@click.option(
    "-n",
    "position",
    type=click.IntRange(min=1),
    default=1,
    help="Nth most recent output.",
)
@click.option(
    "--list", "show_list", is_flag=True, help="Show recent translations with previews."
)
def recall_command(position: int, show_list: bool) -> None:
    db = HistoryDB()
    if show_list:
        previews = db.fetch_recent_translations(limit=10)
        if not previews:
            click.echo("No translation history.", err=True)
            raise click.exceptions.Exit(1)
        _show_translation_previews(previews)
        return

    output_text = db.fetch_recent_output(position)
    if output_text is None:
        _show_recall_missing(db.count_translations())
        raise click.exceptions.Exit(1)
    sys.stdout.write(output_text)


def _template_kwargs(
    terms: tuple[str, ...],
    style: str | None,
    background_context: str | None,
    format_type: str | None,
    instructions: tuple[str, ...],
) -> dict[str, object]:
    return {
        "terms": terms,
        "style": style,
        "background_text": background_context,
        "format_type": format_type,
        "instructions": instructions,
    }


def _announce_tokenizer_download() -> None:
    if has_tokenizer_support() and not TOKENIZER_PATH.exists():
        click.echo("Downloading tokenizer...", err=True)


def _select_document_translation_plan(
    text: str, target_lang: str, assume_yes: bool
) -> DocumentLanguagePlan | None:
    plan = analyze_document_language(text, target_lang)
    if plan.paragraph_count == 0:
        return plan
    if plan.has_mixed_language:
        _show_partial_translation_plan(plan)
        if assume_yes or not sys.stdin.isatty():
            return plan
        sys.stderr.write(
            f"{plan.target_paragraph_count} of {plan.paragraph_count} paragraphs "
            f"are already in {target_lang}. Translate only the remaining "
            f"{plan.translate_paragraph_count} paragraphs? (y/n/all) "
        )
        sys.stderr.flush()
        answer = sys.stdin.readline().strip().lower()
        if answer in {"y", "yes"}:
            return plan
        if answer == "all":
            return plan.translate_all_paragraphs()
        return None
    if plan.target_paragraph_count != plan.paragraph_count:
        return plan
    if assume_yes or not sys.stdin.isatty():
        return plan.translate_all_paragraphs()
    sys.stderr.write(
        f"Input appears to already be in {target_lang}. Translate anyway? (y/n) "
    )
    sys.stderr.flush()
    answer = sys.stdin.readline().strip().lower()
    return plan.translate_all_paragraphs() if answer in {"y", "yes"} else None


def _show_partial_translation_plan(plan: DocumentLanguagePlan) -> None:
    click.echo("Partial translation plan:", err=True)
    for section in plan.sections:
        if section.kind == "paragraph":
            action = "translate" if section.should_translate else "keep"
            detected = section.detected_lang or "unknown"
            click.echo(
                f"  [{section.paragraph_index}] {action} ({detected}): "
                f"{_section_preview(section.text)}",
                err=True,
            )
        elif section.kind == "code":
            click.echo("  [code] keep fenced code block", err=True)


def _section_preview(text: str, limit: int = 64) -> str:
    preview = " ".join(text.split())
    return preview[:limit]


def _stream_enabled(config: HotConfig, override: bool | None) -> bool:
    if override is not None:
        return override
    value = getattr(config, "stream", True)
    return value if isinstance(value, bool) else True


def _show_history_stats(db: HistoryDB) -> None:
    stats = db.stats()
    if stats is None:
        click.echo("No history yet.")
        return
    click.echo(f"Tasks: {stats.count}")
    click.echo(
        "Tokens/sec: "
        f"avg {stats.avg_tokens_per_second:.1f}, "
        f"p50 {stats.median_tokens_per_second:.1f}, "
        f"p5 {stats.p5_tokens_per_second:.1f}, "
        f"p95 {stats.p95_tokens_per_second:.1f}"
    )
    click.echo(f"Avg output tokens/segment: {stats.avg_output_tokens_per_segment:.1f}")
    click.echo(f"Total duration: {format_duration(stats.total_duration_seconds)}")
    click.echo(f"Total input tokens: {stats.total_input_tokens:,}")
    click.echo(f"Total output tokens: {stats.total_output_tokens:,}")


def _show_history_records(records: list[TaskRecord]) -> None:
    if not records:
        click.echo("No history yet.")
        return
    click.echo(
        f"{'ID':>4} {'Finished':<19} {'Target':<8} {'Type':<12} "
        f"{'Seg':>3} {'Conc':>4} {'Tok/s':>7} {'Time':>9}"
    )
    for record in records:
        click.echo(
            f"{record.id or 0:>4} "
            f"{_compact_timestamp(record.finished_at):<19} "
            f"{record.target_lang:<8.8} "
            f"{record.template_type:<12.12} "
            f"{record.segments:>3} "
            f"{record.concurrency:>4} "
            f"{record.tokens_per_second:>7.1f} "
            f"{format_duration(record.duration_seconds):>9}"
        )


def _show_translation_previews(previews: list[TranslationPreview]) -> None:
    click.echo(
        f"{'N':>3} {'ID':>4} {'Finished':<19} {'Target':<8} "
        f"{'Type':<12} {'Chars':>7} Preview"
    )
    for preview in previews:
        click.echo(
            f"{preview.position:>3} "
            f"{preview.id:>4} "
            f"{_compact_timestamp(preview.finished_at):<19} "
            f"{preview.target_lang:<8.8} "
            f"{preview.template_type:<12.12} "
            f"{preview.output_chars:>7} "
            f"{preview.preview}"
        )


def _show_recall_missing(count: int) -> None:
    if count == 0:
        click.echo("No translation history.", err=True)
        return
    click.echo(f"Only {count} translations in history.", err=True)


def _compact_timestamp(value: str) -> str:
    return value.replace("T", " ")[:19]
