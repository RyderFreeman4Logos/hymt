from __future__ import annotations

from pathlib import Path
import asyncio
import os
import shlex
import subprocess
import sys

import click

from hymt.config import HotConfig, config_path, show
from hymt.segment import TOKENIZER_PATH, ensure_tokenizer
from hymt.templates import TemplateType
from hymt.translate import translate_file, translate_text


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
LONG_VALUE_PREFIXES = tuple(f"{option}=" for option in VALUE_OPTIONS if option.startswith("--"))
SHORT_VALUE_OPTIONS = frozenset({"-t", "-f", "-o"})


class TranslationGroup(click.Group):
    def parse_args(self, ctx: click.Context, args: list[str]) -> list[str]:
        if not args and self.no_args_is_help and not ctx.resilient_parsing:
            raise click.NoArgsIsHelpError(ctx)
        if self._first_command_candidate(ctx, args) is None:
            args = self._option_args_first(args)
        rest = click.Command.parse_args(self, ctx, args)
        if rest:
            cmd_name = rest[0]
            command = self.get_command(ctx, cmd_name)
            if command is not None:
                ctx._protected_args, ctx.args = rest[:1], rest[1:]
            else:
                ctx._protected_args, ctx.args = [], rest
        return ctx.args

    def _first_command_candidate(self, ctx: click.Context, args: list[str]) -> str | None:
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


def _is_attached_short_value(arg: str) -> bool:
    return any(arg.startswith(option) and arg != option for option in SHORT_VALUE_OPTIONS)


@click.group(
    cls=TranslationGroup,
    invoke_without_command=True,
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True},
    help="Translate positional text, a file, or stdin.",
)
@click.option("--target", "-t", "target_lang", help="Target language code.")
@click.option("--file", "-f", "input_file", type=click.Path(path_type=Path), help="Input file path.")
@click.option("--output", "-o", "output_file", type=click.Path(path_type=Path), help="Output file path.")
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
@click.option("--context", "background_context", help="Background context for context-aware translations.")
@click.option("--format", "format_type", help="Data format for structured translations.")
@click.option("--instruction", "instructions", multiple=True, help="Personalization instruction.")
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
) -> None:
    if ctx.invoked_subcommand is not None:
        return
    text = " ".join(ctx.args) if ctx.args else None
    if target_lang is None:
        raise click.UsageError("--target / -t is required for translation")
    if input_file is not None and text is not None:
        raise click.UsageError("Use either positional text or --file, not both")

    try:
        kwargs = _template_kwargs(terms, style, background_context, format_type, instructions)
        config = HotConfig()
        selected_type = TemplateType(template_type)
        _announce_tokenizer_download()
        if input_file is not None:
            asyncio.run(translate_file(input_file, output_file, target_lang, config, selected_type, **kwargs))
            return
        source_text = text if text is not None else sys.stdin.read()
        translated = asyncio.run(translate_text(source_text, target_lang, config, selected_type, **kwargs))
    except (OSError, ValueError, RuntimeError) as exc:
        raise click.ClickException(str(exc)) from exc

    if output_file is None:
        click.echo(translated, nl=False)
        return
    output_file.parent.mkdir(parents=True, exist_ok=True)
    output_file.write_text(translated, encoding="utf-8")


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


@main.group()
def tokenizer() -> None:
    pass


@tokenizer.command("download")
@click.option("--force", is_flag=True, help="Force a fresh tokenizer download.")
def tokenizer_download(force: bool) -> None:
    click.echo("Downloading tokenizer...", err=True)
    try:
        path = ensure_tokenizer(force_download=force)
    except OSError as exc:
        raise click.ClickException(str(exc)) from exc
    click.echo(path)


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
    if not TOKENIZER_PATH.exists():
        click.echo("Downloading tokenizer...", err=True)
