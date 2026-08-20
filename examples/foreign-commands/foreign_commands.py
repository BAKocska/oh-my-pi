"""Read prompt-only commands from recognized foreign CLI layouts."""

from __future__ import annotations

import json
import posixpath
import re
from dataclasses import dataclass
from typing import Any, Mapping, Sequence

import omp
from omp import ui

_PLUGIN_ROOT = "${PLUGIN_ROOT}"
_LAYOUT_SUFFIXES = {
    "claude-code": (".claude", "commands"),
    "codex": (".codex", "prompts"),
    "gemini-cli": (".gemini", "commands"),
}
_COMMAND_SUFFIXES = frozenset({".md", ".markdown"})
_MAX_FILE_BYTES = 256 * 1024
_FRONTMATTER = re.compile(r"\A---[ \t]*\r?\n(.*?)\r?\n---[ \t]*(?:\r?\n|\Z)", re.DOTALL)
_FIELD = re.compile(r"^([A-Za-z][A-Za-z0-9_-]*):[ \t]*(.*)$")
_COMMAND_NAME = re.compile(r"^[a-z0-9][a-z0-9._-]*$")
_ARG_TOKEN = re.compile(r"\$(ARGUMENTS|@|[1-9][0-9]*)")


@dataclass(frozen=True, slots=True)
class ForeignCommand:
    """One inert prompt imported from a recognized foreign layout."""

    name: str
    description: str
    body: str
    source: str
    layout: str


@dataclass(frozen=True, slots=True)
class ImportWarning:
    """One refused foreign root or file with a stable diagnostic code."""

    code: str
    path: str
    message: str


@dataclass(frozen=True, slots=True)
class ImportResult:
    """Bounded result of one foreign-command discovery pass."""

    commands: tuple[ForeignCommand, ...]
    warnings: tuple[ImportWarning, ...]


def _setting_roots(settings: Mapping[str, object]) -> tuple[str, ...]:
    value = settings.get("foreign_roots", "[]")
    if isinstance(value, str):
        try:
            value = json.loads(value)
        except json.JSONDecodeError as error:
            raise ValueError("foreign_roots must be a JSON array of paths") from error
    if not isinstance(value, Sequence) or isinstance(value, (str, bytes, bytearray)):
        raise TypeError("foreign_roots must be a sequence of paths")
    roots = tuple(value)
    if any(not isinstance(root, str) or not root.strip() for root in roots):
        raise TypeError("foreign_roots entries must be non-empty strings")
    return roots


def _contained(path: str, root: str) -> bool:
    path = posixpath.normpath(path)
    root = posixpath.normpath(root)
    return path == root or path.startswith(root.rstrip("/") + "/")


def _expand_root(spec: str, plugin_root: object) -> tuple[str | None, ImportWarning | None]:
    if _PLUGIN_ROOT not in spec:
        if "${" in spec:
            return None, ImportWarning("W-PATH-ESCAPE", spec, "unsupported path variable")
        return posixpath.normpath(spec), None
    if spec.count(_PLUGIN_ROOT) != 1 or not isinstance(plugin_root, str) or not plugin_root:
        return None, ImportWarning("W-PATH-ESCAPE", spec, "PLUGIN_ROOT is missing or ambiguous")
    base = posixpath.normpath(plugin_root)
    expanded = posixpath.normpath(spec.replace(_PLUGIN_ROOT, base))
    if not _contained(expanded, base):
        return None, ImportWarning("W-PATH-ESCAPE", spec, "PLUGIN_ROOT expansion escaped its root")
    return expanded, None


def _layout(path: str) -> str | None:
    parts = tuple(part for part in posixpath.normpath(path).split("/") if part)
    for name, suffix in _LAYOUT_SUFFIXES.items():
        if parts[-len(suffix) :] == suffix:
            return name
    return None


def _entry_path(entry: object) -> str:
    value = getattr(entry, "path", entry)
    return str(value)


def _entry_kind(entry: object) -> str:
    value = getattr(entry, "kind", "")
    return str(getattr(value, "value", value))


def _parse_frontmatter(text: str) -> tuple[dict[str, str], str]:
    match = _FRONTMATTER.match(text)
    if match is None:
        return {}, text.strip()
    fields: dict[str, str] = {}
    for line in match.group(1).splitlines():
        parsed = _FIELD.match(line)
        if parsed is None:
            continue
        value = parsed.group(2).strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        fields[parsed.group(1).lower()] = value
    return fields, text[match.end() :].strip()


def _command_from_text(path: str, layout: str, text: str) -> ForeignCommand:
    fields, body = _parse_frontmatter(text)
    filename = posixpath.basename(path)
    stem = filename.rsplit(".", 1)[0].lower()
    name = fields.get("name", stem).strip().lower().replace(" ", "-")
    if not _COMMAND_NAME.fullmatch(name):
        raise ValueError(f"invalid foreign command name {name!r}")
    if not body:
        raise ValueError("foreign command body is empty")
    summary = fields.get("description", f"Imported prompt {name}").strip()
    provenance = f"[foreign:{layout}] {summary} — content-only import from {path}"
    return ForeignCommand(name, provenance, body, path, layout)


async def discover_foreign_commands(settings: Mapping[str, object]) -> ImportResult:
    """Discover inert prompt files without executing any foreign code."""

    omp.env.require(omp.env.Capability.FS_READ, omp.env.Capability.DOC_READ)
    commands: list[ForeignCommand] = []
    warnings: list[ImportWarning] = []
    seen: set[str] = set()
    plugin_root = settings.get("plugin_root", "")

    for spec in _setting_roots(settings):
        expanded, warning = _expand_root(spec, plugin_root)
        if warning is not None:
            warnings.append(warning)
            continue
        assert expanded is not None
        layout = _layout(expanded)
        if layout is None:
            warnings.append(ImportWarning("W-FOREIGN-ROOT", expanded, "unrecognized foreign command layout; skipped"))
            continue
        root = str(await omp.env.fs.canonicalize(omp.EnvPath(expanded)))
        if _PLUGIN_ROOT in spec and not _contained(root, posixpath.normpath(str(plugin_root))):
            warnings.append(ImportWarning("W-PATH-ESCAPE", expanded, "canonical root escaped PLUGIN_ROOT"))
            continue

        pending = [root]
        while pending:
            directory = pending.pop()
            for entry in await omp.env.fs.list_dir(omp.EnvPath(directory)):
                candidate = _entry_path(entry)
                canonical = str(await omp.env.fs.canonicalize(omp.EnvPath(candidate)))
                if not _contained(canonical, root):
                    warnings.append(ImportWarning("W-PATH-ESCAPE", candidate, "entry escaped the configured root"))
                    continue
                kind = _entry_kind(entry)
                if kind == omp.env.FileKind.DIRECTORY.value:
                    pending.append(canonical)
                    continue
                if kind != omp.env.FileKind.REGULAR_FILE.value:
                    continue
                suffix = posixpath.splitext(canonical)[1].lower()
                if suffix not in _COMMAND_SUFFIXES:
                    continue
                try:
                    async with await omp.env.docs.open(omp.EnvPath(canonical)) as doc:
                        data = await doc.read_bytes()
                    if len(data) > _MAX_FILE_BYTES:
                        raise ValueError("foreign command exceeds 256 KiB")
                    command = _command_from_text(canonical, layout, data.decode("utf-8"))
                    if command.name in seen:
                        raise ValueError(f"duplicate foreign command name {command.name!r}")
                except (UnicodeDecodeError, ValueError) as error:
                    warnings.append(ImportWarning("W-FOREIGN-COMMAND", canonical, str(error)))
                    continue
                seen.add(command.name)
                commands.append(command)

    commands.sort(key=lambda command: (command.name, command.source))
    return ImportResult(tuple(commands), tuple(warnings))


def _expand_arguments(body: str, invocation: ui.Invocation) -> str:
    argv = invocation.argv

    def replace(match: re.Match[str]) -> str:
        token = match.group(1)
        if token in {"ARGUMENTS", "@"}:
            return invocation.raw
        index = int(token) - 1
        return argv[index] if index < len(argv) else ""

    return _ARG_TOKEN.sub(replace, body)


def register_foreign_commands(commands: Sequence[ForeignCommand]) -> None:
    """Add discovered prompt handlers to the current declaration collector."""

    def handler(imported: ForeignCommand) -> Any:
        async def invoke(invocation: ui.Invocation, ctx: omp.Context) -> ui.Prompt:
            """Return imported prompt content without running foreign code."""

            del ctx
            return ui.Prompt(_expand_arguments(imported.body, invocation))

        return invoke

    for command in commands:
        omp.command(command.name, description=command.description)(handler(command))


@omp.hook("extension_activate")
async def load_foreign_commands(event: omp.ExtensionActivateEvent, ctx: omp.Context) -> None:
    """Discover configured foreign command content when this extension activates."""

    del event
    result = await discover_foreign_commands(ctx.settings)
    for warning in result.warnings:
        ctx.log("warning", warning.message, code=warning.code, path=warning.path)
    register_foreign_commands(result.commands)
