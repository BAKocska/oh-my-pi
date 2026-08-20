from __future__ import annotations

import re
from dataclasses import dataclass

import omp
from omp import ui


_MAX_TEMPLATES = 128
_MAX_CHAIN_STEPS = 8
_PLACEHOLDER = re.compile(r"\$(ARGUMENTS|@|[1-9][0-9]*)")


class TemplateError(ValueError):
    """Report invalid prompt-template content."""


@dataclass(frozen=True, slots=True)
class _TemplateRef:
    name: str
    path: omp.EnvPath


@dataclass(frozen=True, slots=True)
class _Template:
    name: str
    body: str
    description: str
    role: str
    thinking: omp.agents.ThinkingLevel | None
    args: str
    submit: bool
    chain: int


def _scalar(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {'"', "'"}:
        return value[1:-1]
    return value


def _frontmatter(text: str) -> tuple[dict[str, str], str]:
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return {}, text

    try:
        closing = next(
            index for index, line in enumerate(lines[1:], start=1) if line.strip() == "---"
        )
    except StopIteration as error:
        raise TemplateError("frontmatter is missing its closing ---") from error

    values: dict[str, str] = {}
    allowed = {"description", "role", "thinking", "args", "submit", "chain"}
    for number, line in enumerate(lines[1:closing], start=2):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if ":" not in line:
            raise TemplateError(f"frontmatter line {number} must be key: value")
        key, raw = line.split(":", 1)
        key = key.strip()
        if key not in allowed:
            raise TemplateError(f"unknown frontmatter key: {key}")
        if key in values:
            raise TemplateError(f"duplicate frontmatter key: {key}")
        values[key] = _scalar(raw)

    body = "\n".join(lines[closing + 1 :]).strip()
    if not body:
        raise TemplateError("template body must not be empty")
    return values, body


def _parse_template(name: str, text: str) -> _Template:
    values, body = _frontmatter(text)
    role = values.get("role", "task").strip()
    if not role:
        raise TemplateError("frontmatter role must not be empty")

    thinking_text = values.get("thinking", "").strip().lower()
    try:
        thinking = (
            omp.agents.ThinkingLevel(thinking_text) if thinking_text else None
        )
    except ValueError as error:
        choices = ", ".join(level.value for level in omp.agents.ThinkingLevel)
        raise TemplateError(f"thinking must be one of: {choices}") from error

    submit_text = values.get("submit", "true").strip().lower()
    if submit_text not in {"true", "false"}:
        raise TemplateError("submit must be true or false")
    submit = submit_text == "true"

    chain_text = values.get("chain", "0").strip()
    try:
        chain = int(chain_text)
    except ValueError as error:
        raise TemplateError("chain must be an integer") from error
    if not 0 <= chain <= _MAX_CHAIN_STEPS:
        raise TemplateError(f"chain must be between 0 and {_MAX_CHAIN_STEPS}")

    description = values.get("description", "").strip()
    if not description:
        description = next(
            (line.strip().lstrip("# ").strip() for line in body.splitlines() if line.strip()),
            name,
        )[:72]

    return _Template(
        name=name,
        body=body,
        description=description,
        role=role,
        thinking=thinking,
        args=values.get("args", "").strip(),
        submit=submit,
        chain=chain,
    )


def _expand(text: str, arguments: tuple[str, ...]) -> str:
    referenced: set[int] = set()

    def replace(match: re.Match[str]) -> str:
        token = match.group(1)
        if token in {"@", "ARGUMENTS"}:
            referenced.update(range(len(arguments)))
            return " ".join(arguments)
        index = int(token) - 1
        if index < len(arguments):
            referenced.add(index)
            return arguments[index]
        return ""

    expanded = _PLACEHOLDER.sub(replace, text)
    trailing = [value for index, value in enumerate(arguments) if index not in referenced]
    if trailing:
        expanded = f"{expanded.rstrip()}\n\n{' '.join(trailing)}"
    return expanded


def _template_root(ctx: omp.Context) -> omp.EnvPath:
    configured = ctx.settings.get("templates_dir", ".omp/templates")
    if not isinstance(configured, str) or not configured.strip():
        raise TemplateError("templates_dir must be a non-empty string")
    return omp.EnvPath(configured.strip())


async def _template_refs(ctx: omp.Context) -> dict[str, _TemplateRef]:
    root = _template_root(ctx)
    entries = await omp.env.find.files(
        root=root,
        glob=("*.md", "**/*.md"),
        hidden=False,
        gitignore=True,
        limit=_MAX_TEMPLATES,
    )
    root_text = str(root).rstrip("/")
    prefix = f"{root_text}/" if root_text else ""
    refs: dict[str, _TemplateRef] = {}
    for entry in entries:
        path_text = str(entry.path)
        relative = path_text[len(prefix) :] if prefix and path_text.startswith(prefix) else path_text
        if not relative.endswith(".md"):
            continue
        name = relative[:-3].replace("/", ":")
        if name in refs:
            raise TemplateError(f"duplicate template name: {name}")
        refs[name] = _TemplateRef(name, entry.path)
    return refs


async def _load_named(name: str, ctx: omp.Context) -> _Template | None:
    reference = (await _template_refs(ctx)).get(name)
    if reference is None:
        return None
    return _parse_template(name, await reference.path.read_text())


async def _complete_templates(
    query: ui.ArgQuery, ctx: omp.Context
) -> tuple[ui.CompletionItem, ...]:
    if query.argv:
        return ()
    prefix = query.prefix.casefold()
    rows: list[ui.CompletionItem] = []
    for name, reference in sorted((await _template_refs(ctx)).items()):
        if not name.casefold().startswith(prefix):
            continue
        try:
            template = _parse_template(name, await reference.path.read_text())
        except (omp.env.EnvError, TemplateError, UnicodeError):
            continue
        rows.append(
            ui.CompletionItem(
                insert=name,
                label=name,
                desc=template.description,
                hint=template.args or None,
                group="Prompt templates",
            )
        )
    return tuple(rows)


async def _run_chain(template: _Template, prompt: str) -> str | None:
    current = prompt
    for _step in range(template.chain):
        handle = await omp.agents.spawn(
            omp.agents.SubagentSpec(
                task=current,
                agent=template.role,
                thinking=template.thinking,
                max_depth=0,
            )
        )
        result = await handle.wait()
        if result.status is not omp.agents.RunStatus.COMPLETED:
            return None
        current = result.text
    return current


@ui.command(
    "tpl",
    description="Run a frontmatter-configured Markdown prompt template",
    args=(ui.Arg("template", "Template name", usage="[arguments...]"),),
    hint="<template> [arguments...]",
    arg_completions=_complete_templates,
)
async def tpl(inv: ui.Invocation, ctx: omp.Context) -> ui.Consumed | ui.Prompt:
    """Populate, submit, or chain one Markdown prompt template."""

    if not inv.argv:
        return ui.Consumed(ui.text("Usage: /tpl <template> [arguments...]"))
    try:
        template = await _load_named(inv.argv[0], ctx)
    except (omp.env.EnvError, TemplateError, UnicodeError) as error:
        return ui.Consumed(ui.text(f"Template error: {error}"))
    if template is None:
        return ui.Consumed(ui.text(f"Unknown template: {inv.argv[0]}"))

    expanded = _expand(template.body, inv.argv[1:])
    if template.chain:
        expanded = await _run_chain(template, expanded)
        if expanded is None:
            return ui.Consumed(ui.text(f"Template chain failed: {template.name}"))
    return ui.Prompt(expanded, submit=template.submit)
