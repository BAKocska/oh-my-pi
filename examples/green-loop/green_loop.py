from __future__ import annotations

import fnmatch
import hashlib
import re
import shlex
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any

import omp

_SCOPE = omp.StateScope.SESSION
_SCHEDULE_PROMPT = "[green-loop:run-affected-tests]"
_MAX_PATHS = 256
_MAX_PATH_BYTES = 1_024
_MAX_COMMAND_BYTES = 16_384
_MAX_SUMMARY_BYTES = 3_072
_ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
_VOLATILE_DURATION = re.compile(r"\b\d+(?:\.\d+)?(?:ms|s)\b")
_PATCH_HEADER = re.compile(r"^\[([^#\]\r\n]+)#[0-9A-Fa-f]{4}\]$")

_DEFAULT_AFFECTED: Mapping[str, Mapping[str, object]] = {
    "node": {
        "patterns": ("*.js", "*.jsx", "*.mjs", "*.cjs", "*.ts", "*.tsx"),
        "command": "npm test -- --findRelatedTests {paths}",
    },
    "python": {
        "patterns": ("*.py",),
        "command": "python3 -m pytest {paths}",
    },
    "go": {"patterns": ("*.go",), "command": "go test ./..."},
    "rust": {"patterns": ("*.rs",), "command": "cargo test"},
    "make": {"patterns": ("Makefile", "*.mk"), "command": "make test"},
}


@omp.entry_kind("examples.green-loop.touched", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class _TouchedPaths:
    paths: tuple[str, ...]


@omp.entry_kind("examples.green-loop.run", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class _GreenRun:
    paths: tuple[str, ...]
    command: str
    passed: bool
    exit_code: int | None
    failure_digest: str | None
    summary: str


@dataclass(frozen=True, slots=True)
class _RunReport:
    ran: bool
    passed: bool | None
    paths: tuple[str, ...]
    command: str
    summary: str
    notified: bool


def _bounded(text: str, maximum: int) -> str:
    encoded = text.encode("utf-8")
    if len(encoded) <= maximum:
        return text
    return encoded[-maximum:].decode("utf-8", errors="ignore")


def _clean_path(value: object) -> str | None:
    if not isinstance(value, str):
        return None
    path = value.strip()
    if (
        not path
        or path.startswith(("/", "artifact://", "http://", "https://"))
        or "\x00" in path
        or len(path.encode("utf-8")) > _MAX_PATH_BYTES
    ):
        return None
    return path.removeprefix("./")


def _paths_in_patch(patch: object) -> tuple[str, ...]:
    if not isinstance(patch, str):
        return ()
    paths: list[str] = []
    for line in patch.splitlines():
        header = _PATCH_HEADER.fullmatch(line)
        if header:
            paths.append(header.group(1))
            continue
        if line.startswith("MV "):
            try:
                moved = shlex.split(line[3:])
            except ValueError:
                continue
            if len(moved) == 1:
                paths.append(moved[0])
    return tuple(paths)


def _touched_paths(event: omp.ToolResultEvent) -> tuple[str, ...]:
    values: list[object] = []
    for source in (event.target.args, event.payload or {}):
        for key in ("path", "previous_path"):
            if key in source:
                values.append(source[key])
        for key in ("paths", "changed", "created"):
            candidate = source.get(key)
            if isinstance(candidate, Sequence) and not isinstance(candidate, (str, bytes)):
                values.extend(candidate)
        values.extend(_paths_in_patch(source.get("patch")))

    paths = {
        path
        for value in values
        if (path := _clean_path(value)) is not None
    }
    return tuple(sorted(paths))[:_MAX_PATHS]


def _configured_mapping(settings: Mapping[str, object]) -> Mapping[str, object]:
    affected = settings.get("affected")
    return affected if isinstance(affected, Mapping) else _DEFAULT_AFFECTED


def _render_command(template: str, paths: Sequence[str]) -> str:
    joined = " ".join(shlex.quote(path) for path in paths)
    return template.replace("{paths}", joined)


def _affected_command(paths: tuple[str, ...], settings: Mapping[str, object]) -> str:
    runner = settings.get("test_runner", "")
    if isinstance(runner, str) and runner.strip():
        command = _render_command(runner.strip(), paths)
    else:
        commands: list[str] = []
        for value in _configured_mapping(settings).values():
            if not isinstance(value, Mapping):
                continue
            patterns = value.get("patterns", ())
            template = value.get("command", "")
            if (
                not isinstance(patterns, Sequence)
                or isinstance(patterns, (str, bytes))
                or not isinstance(template, str)
                or not template.strip()
            ):
                continue
            matched = tuple(
                path
                for path in paths
                if any(isinstance(pattern, str) and fnmatch.fnmatch(path, pattern) for pattern in patterns)
            )
            if not matched:
                continue
            rendered = _render_command(template.strip(), matched)
            if rendered not in commands:
                commands.append(rendered)
        command = " && ".join(f"({item})" for item in commands)

    if len(command.encode("utf-8")) > _MAX_COMMAND_BYTES:
        raise ValueError("affected-test command exceeds 16 KiB")
    return command


def _failure_digest(command: str, exit_code: int | None, output: str) -> str:
    stable = _VOLATILE_DURATION.sub("<duration>", _ANSI.sub("", output))
    material = f"{command}\0{exit_code}\0{_bounded(stable, 16_384)}".encode()
    return hashlib.sha256(material).hexdigest()[:24]


def _paint_badge(passed: bool) -> None:
    tone = omp.ui.Token.OK if passed else omp.ui.Token.ERR
    label = "tests green" if passed else "tests red"
    omp.ui.set_status(
        "green-loop",
        omp.ui.tml("<segment fg={tone}>{label}</segment>", tone=tone, label=omp.ui.text(label)),
        order=80,
        side=omp.ui.Slot.STATUS_RIGHT,
    )


async def _pending_paths() -> tuple[tuple[str, ...], object | None]:
    previous = await omp.state.latest(_GreenRun, scope=_SCOPE)
    watermark = None if previous is None else previous.id
    records = await omp.state.entries(_TouchedPaths, scope=_SCOPE, since=watermark)
    paths = {
        path
        for record in records
        if isinstance(record.value, _TouchedPaths)
        for path in record.value.paths
    }
    return tuple(sorted(paths))[:_MAX_PATHS], previous


async def _run_green(ctx: omp.Context) -> _RunReport:
    paths, previous = await _pending_paths()
    command = _affected_command(paths, ctx.settings)
    if not command:
        return _RunReport(False, None, paths, "", "No affected test command matched.", False)

    timeout = omp.Duration(str(ctx.settings.get("timeout", "10m")))
    completed = await omp.env.sh.run(command, timeout=timeout)
    output = completed.text()
    passed = completed.outcome is omp.env.Outcome.EXITED and completed.exit_code == 0
    summary = _bounded(output.strip() or ("affected tests passed" if passed else "affected tests failed"), _MAX_SUMMARY_BYTES)
    digest = None if passed else _failure_digest(command, completed.exit_code, output)
    prior_value = None if previous is None else previous.value
    duplicate = (
        not passed
        and isinstance(prior_value, _GreenRun)
        and prior_value.failure_digest == digest
    )

    await omp.state.append(
        _GreenRun(paths, command, passed, completed.exit_code, digest, summary),
        scope=_SCOPE,
        idempotency_key=f"green-loop-run:{ctx.invocation}",
    )
    _paint_badge(passed)

    notified = False
    if not passed and not duplicate:
        notice = _bounded(
            f"Affected tests failed ({digest}).\nCommand: {command}\n{summary}",
            _MAX_SUMMARY_BYTES,
        )
        await omp.agents.inject(
            notice,
            mode=omp.agents.DeliveryMode.NEXT_TURN,
            visible=True,
            role="system",
        )
        notified = True
    return _RunReport(True, passed, paths, command, summary, notified)


@omp.hook(
    "tool_result",
    phase=omp.HookPhase.OBSERVE,
    when=omp.When(
        target=frozenset({omp.TargetKind.CORE}),
        name=frozenset({"edit", "write"}),
    ),
)
async def remember_touched(event: omp.ToolResultEvent, ctx: omp.Context) -> None:
    """Append paths from each settled successful core edit or write."""
    del ctx
    if event.outcome is not omp.OutcomeKind.OK or not isinstance(event.target, omp.CoreTool):
        return
    paths = _touched_paths(event)
    if paths:
        await omp.state.append(
            _TouchedPaths(paths),
            scope=_SCOPE,
            idempotency_key=f"green-loop-touch:{event.call_id}",
        )


@omp.hook("extension_activate")
async def arm_after_idle(event: omp.ExtensionActivateEvent, ctx: omp.Context) -> None:
    """Upsert the session-scoped AfterIdle test firing and restore its badge."""
    del event
    await omp.agents.schedule(
        "green-loop-after-idle",
        omp.agents.AfterIdle(omp.Duration(str(ctx.settings.get("idle", "20s")))),
        omp.agents.Inject(
            _SCHEDULE_PROMPT,
            mode=omp.agents.DeliveryMode.NEXT_TURN,
            visible=False,
        ),
        scope=omp.agents.ScheduleScope.SESSION,
        missed=omp.agents.MissedRunPolicy.SKIP,
        overlap="skip",
    )
    previous = await omp.state.latest(_GreenRun, scope=_SCOPE)
    if previous is not None and isinstance(previous.value, _GreenRun):
        _paint_badge(previous.value.passed)


@omp.hook("before_agent_start", phase=omp.HookPhase.REVIEW)
async def run_after_idle(event: omp.BeforeAgentStartEvent, ctx: omp.Context) -> omp.Deny | None:
    """Consume this extension's scheduled sentinel after running affected tests."""
    if (
        event.source is not omp.InputSource.SCHEDULE
        or event.schedule_id is None
        or event.text != _SCHEDULE_PROMPT
    ):
        return None
    await _run_green(ctx)
    return omp.Deny("green-loop schedule handled", code="green_loop_scheduled")


@omp.command("green", description="Run tests affected by successful edits and writes.")
async def green(inv: omp.ui.Invocation, ctx: omp.Context) -> omp.ui.Consumed:
    """Run the affected-test fold immediately and report its latest result."""
    del inv
    report = await _run_green(ctx)
    if not report.ran:
        return omp.ui.Consumed(notice=omp.ui.text(report.summary))
    state = "passed" if report.passed else "failed"
    return omp.ui.Consumed(
        notice=omp.ui.text(
            f"Affected tests {state} for {len(report.paths)} touched path(s)."
        )
    )
