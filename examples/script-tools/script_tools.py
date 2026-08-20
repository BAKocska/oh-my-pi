from __future__ import annotations

import asyncio
import re
from collections.abc import Mapping
from dataclasses import dataclass
from typing import Any

import omp


_PARENT = omp.devices.parent("scripts", family="script-tools", rev=1, place="host")
_HEADER_ARG = re.compile(
    r"^\s*#\s*@arg\s+([a-z][a-z0-9_]*)\s+(string|integer|number|boolean)\s+(.+?)\s*$"
)
_HEADER_DESCRIBE = re.compile(r"^\s*#\s*@describe\s+(.+?)\s*$")
_SEGMENT = re.compile(r"[^a-z0-9_]+")
_RESCAN_KINDS = frozenset(
    {
        "committed",
        "external_created",
        "external_modified",
        "external_deleted",
        "external_renamed",
        "watch_rescanned",
    }
)
_ACTIVE: set[str] = set()
_WATCHERS: dict[str, asyncio.Task[None]] = {}
_REFRESH_LOCK = asyncio.Lock()


@dataclass(frozen=True, slots=True)
class _Argument:
    name: str
    kind: str
    help: str


@dataclass(frozen=True, slots=True)
class _Script:
    path: omp.EnvPath
    subpath: str
    description: str
    arguments: tuple[_Argument, ...]


def _parse_header(path: omp.EnvPath, source: str) -> tuple[str, tuple[_Argument, ...]]:
    description: str | None = None
    arguments: list[_Argument] = []
    names: set[str] = set()
    for line in source.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#!"):
            continue
        describe = _HEADER_DESCRIBE.match(line)
        if describe is not None:
            if description is not None:
                raise ValueError(f"{path}: duplicate # @describe header")
            description = describe.group(1)
            continue
        argument = _HEADER_ARG.match(line)
        if argument is not None:
            name, kind, help_text = argument.groups()
            if name in names:
                raise ValueError(f"{path}: duplicate # @arg {name!r}")
            names.add(name)
            arguments.append(_Argument(name, kind, help_text))
            continue
        if stripped.startswith("# @arg") or stripped.startswith("# @describe"):
            raise ValueError(f"{path}: malformed script-tool header {stripped!r}")
        if stripped.startswith("#"):
            continue
        break
    if description is None:
        raise ValueError(f"{path}: missing # @describe header")
    return description, tuple(arguments)


def _normalize_segment(value: str) -> str:
    segment = _SEGMENT.sub("_", value.lower()).strip("_")
    if not segment:
        raise ValueError(f"script path segment {value!r} has no device-safe characters")
    if segment[0].isdigit():
        segment = f"script_{segment}"
    return segment[:64]


def _script_subpath(root: omp.EnvPath, path: omp.EnvPath) -> str:
    root_text = str(root).rstrip("/")
    path_text = str(path)
    prefix = f"{root_text}/" if root_text and root_text != "." else ""
    if prefix and not path_text.startswith(prefix):
        raise ValueError(f"script {path} is not beneath configured root {root}")
    relative = path_text[len(prefix) :] if prefix else path_text
    parts = relative.split("/")
    if parts[-1].lower().endswith(".sh"):
        parts[-1] = parts[-1][:-3]
    return "/".join(_normalize_segment(part) for part in parts)


def _schema(script: _Script) -> dict[str, object]:
    properties = {
        argument.name: {
            "type": argument.kind,
            "description": argument.help,
        }
        for argument in script.arguments
    }
    return {
        "type": "object",
        "properties": properties,
        "required": [argument.name for argument in script.arguments],
        "additionalProperties": False,
    }


def _argument_text(argument: _Argument, value: object) -> str:
    if argument.kind == "string":
        if not isinstance(value, str):
            raise TypeError(f"argument {argument.name!r} must be a string")
        return value
    if argument.kind == "integer":
        if isinstance(value, bool) or not isinstance(value, int):
            raise TypeError(f"argument {argument.name!r} must be an integer")
        return str(value)
    if argument.kind == "number":
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise TypeError(f"argument {argument.name!r} must be a number")
        return str(value)
    if not isinstance(value, bool):
        raise TypeError(f"argument {argument.name!r} must be a boolean")
    return "true" if value else "false"


def _command(argument_count: int) -> str:
    variables = [f"OMP_SCRIPT_ARG_{index}" for index in range(argument_count)]
    positional = " ".join(f'"${{{name}}}"' for name in variables)
    unset = " ".join(("OMP_SCRIPT_PATH", *variables))
    return (
        f"set -- {positional}\n"
        "_omp_script_path=$OMP_SCRIPT_PATH\n"
        f"unset {unset}\n"
        'exec "$_omp_script_path" "$@"'
    )


async def _invoke(script: _Script, args: Mapping[str, object]) -> object:
    expected = {argument.name for argument in script.arguments}
    if set(args) != expected:
        missing = sorted(expected - set(args))
        extra = sorted(set(args) - expected)
        raise ValueError(f"invalid arguments: missing={missing!r}, extra={extra!r}")
    environment = {"OMP_SCRIPT_PATH": str(script.path)}
    for index, argument in enumerate(script.arguments):
        environment[f"OMP_SCRIPT_ARG_{index}"] = _argument_text(
            argument, args[argument.name]
        )
    return await omp.env.sh.run(_command(len(script.arguments)), env=environment)


def _body(script: _Script):
    async def invoke(args: dict[str, object], ctx: omp.Context) -> object:
        del ctx
        return await _invoke(script, args)

    invoke.__name__ = _normalize_segment(script.subpath.rsplit("/", 1)[-1])
    invoke.__doc__ = script.description
    return invoke


async def _discover(root: omp.EnvPath) -> tuple[_Script, ...]:
    scripts: list[_Script] = []
    seen: dict[str, omp.EnvPath] = {}
    async for entry in omp.env.find.walk(
        root=root,
        gitignore=True,
        follow=omp.env.Follow.NEVER,
    ):
        kind = getattr(entry.kind, "value", entry.kind)
        if kind not in {"file", "regular_file"} or not str(entry.path).lower().endswith(".sh"):
            continue
        description, arguments = _parse_header(
            entry.path, await entry.path.read_text()
        )
        subpath = _script_subpath(root, entry.path)
        previous = seen.get(subpath)
        if previous is not None:
            raise ValueError(
                f"scripts {previous} and {entry.path} normalize to the same device path"
            )
        seen[subpath] = entry.path
        scripts.append(_Script(entry.path, subpath, description, arguments))
    scripts.sort(key=lambda script: script.subpath)
    return tuple(scripts)


async def _refresh(root: omp.EnvPath) -> tuple[_Script, ...]:
    global _ACTIVE
    async with _REFRESH_LOCK:
        scripts = await _discover(root)
        if scripts:
            await _PARENT.mount_many(
                *(
                    omp.MountSpec(
                        subpath=script.subpath,
                        body=_body(script),
                        schema=_schema(script),
                        summary=script.description,
                        docs=script.description,
                    )
                    for script in scripts
                )
            )
        current = {_PARENT.path(script.subpath) for script in scripts}
        deltas = [
            *(omp.AvailabilityDelta(path, False, "script removed") for path in sorted(_ACTIVE - current)),
            *(omp.AvailabilityDelta(path, True) for path in sorted(current - _ACTIVE)),
        ]
        if deltas:
            await omp.devices.set_availability(*deltas)
        _ACTIVE = current
        return scripts


def _watch_done(key: str, task: asyncio.Task[None]) -> None:
    if _WATCHERS.get(key) is task:
        _WATCHERS.pop(key, None)


async def _watch_script(script: _Script, root: omp.EnvPath) -> None:
    async with await omp.env.docs.open(script.path) as document:
        async for event in document.events():
            kind = getattr(event.kind, "value", event.kind)
            if kind not in _RESCAN_KINDS:
                continue
            scripts = await _refresh(root)
            _sync_watchers(scripts, root, current=asyncio.current_task())


def _sync_watchers(
    scripts: tuple[_Script, ...],
    root: omp.EnvPath,
    *,
    current: asyncio.Task[Any] | None = None,
) -> None:
    desired = {str(script.path): script for script in scripts}
    for key, task in tuple(_WATCHERS.items()):
        if key not in desired and task is not current:
            _WATCHERS.pop(key, None)
            task.cancel()
    for key, script in desired.items():
        task = _WATCHERS.get(key)
        if task is not None and not task.done():
            continue
        task = asyncio.create_task(
            _watch_script(script, root), name=f"script-tools:{script.subpath}"
        )
        _WATCHERS[key] = task
        task.add_done_callback(lambda done, watched=key: _watch_done(watched, done))


@omp.hook("extension_activate")
async def extension_activate(
    event: omp.ExtensionActivateEvent, ctx: omp.Context
) -> None:
    """Discover, mount, and watch configured workspace scripts on activation."""
    del event
    omp.env.require(
        omp.env.Capability.DOC_READ,
        omp.env.Capability.EXEC,
        omp.env.Capability.SEARCH,
    )
    setting = ctx.settings.get("scripts_dir", ".omp/scripts")
    if not isinstance(setting, str) or not setting.strip():
        raise TypeError("scripts_dir must be a non-empty string")
    root = omp.EnvPath(setting)
    for task in tuple(_WATCHERS.values()):
        task.cancel()
    _WATCHERS.clear()
    scripts = await _refresh(root)
    _sync_watchers(scripts, root)
