"""Run declared repository rules and capture settled decision-path changes."""

from __future__ import annotations

import fnmatch
import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass

import omp

_MAX_COMMANDS = 16
_MAX_COMMAND_BYTES = 16_384
_MAX_PATHS = 128
_MAX_PATH_BYTES = 1_024
_MAX_SUMMARY_BYTES = 4_096
_DEFAULT_COMMANDS: Mapping[str, str] = {
    "unmerged": "git diff --quiet --diff-filter=U",
    "whitespace": "git diff --check",
}
_DEFAULT_DECISION_PATHS = (
    "AGENTS.md",
    "adr/**",
    "decisions/**",
    "docs/adr/**",
    "docs/decisions/**",
)
_PATCH_HEADER = re.compile(r"^\[([^#\]\r\n]+)#[0-9A-Fa-f]{4}\]$")


@omp.entry_kind("examples.rules-sync.decision-touch", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class DecisionTouch:
    """Record successful writes to declared decision paths."""

    call_id: str
    paths: tuple[str, ...]


@omp.entry_kind("examples.rules-sync.decision", rev="adr.1", display=False)
@dataclass(frozen=True, slots=True)
class DecisionRecord:
    """Record an ADR-shaped fact at a settled submission boundary."""

    submission_id: str
    title: str
    status: str
    context: str
    decision: str
    consequences: str
    paths: tuple[str, ...]
    settle_reason: str


@dataclass(frozen=True, slots=True)
class RulesCheckArgs:
    """Select declared checks by name, or all checks when empty."""

    only: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class CheckResult:
    """Describe one completed declared conformity command."""

    name: str
    command: str
    passed: bool
    exit_code: int | None
    summary: str


@dataclass(frozen=True, slots=True)
class Finding:
    """Describe one failed conformity check."""

    check: str
    exit_code: int | None
    detail: str


@dataclass(frozen=True, slots=True)
class RulesCheckPayload(omp.Payload):
    """Return all check receipts and the failed-check findings."""

    passed: bool
    checks: tuple[CheckResult, ...]
    findings: tuple[Finding, ...]


def _bounded(value: str, maximum: int) -> str:
    """Bound a UTF-8 string without splitting a code point."""

    encoded = value.encode("utf-8")
    if len(encoded) <= maximum:
        return value
    return encoded[:maximum].decode("utf-8", errors="ignore")


def _commands(settings: Mapping[str, object]) -> tuple[tuple[str, str], ...]:
    """Return the deterministic declared command list."""

    raw = settings.get("commands", _DEFAULT_COMMANDS)
    if not isinstance(raw, Mapping):
        raise ValueError("settings.commands must be a table")
    commands: list[tuple[str, str]] = []
    for name, command in raw.items():
        if (
            not isinstance(name, str)
            or not name
            or not isinstance(command, str)
            or not command.strip()
        ):
            raise ValueError("settings.commands must map non-empty names to scripts")
        script = command.strip()
        if len(script.encode("utf-8")) > _MAX_COMMAND_BYTES:
            raise ValueError(f"declared command {name!r} exceeds 16 KiB")
        commands.append((name, script))
    commands.sort(key=lambda row: row[0])
    if not commands or len(commands) > _MAX_COMMANDS:
        raise ValueError(
            f"settings.commands must declare between 1 and {_MAX_COMMANDS} checks"
        )
    return tuple(commands)


def _selected_commands(
    args: RulesCheckArgs, settings: Mapping[str, object]
) -> tuple[tuple[str, str], ...]:
    """Resolve requested names without admitting caller-supplied shell text."""

    declared = _commands(settings)
    if not args.only:
        return declared
    requested = frozenset(args.only)
    unknown = requested.difference(name for name, _ in declared)
    if unknown:
        raise ValueError(f"unknown declared checks: {', '.join(sorted(unknown))}")
    return tuple(row for row in declared if row[0] in requested)


@omp.device(
    "rules_check",
    family="rules",
    rev=1,
    place="host",
    summary="Run the repository's declared conformity checks.",
    tier=omp.policy.Tier.EXEC,
)
async def rules_check(args: RulesCheckArgs, ctx: omp.Context) -> RulesCheckPayload:
    """Run only manifest-declared conformity commands through the Environment."""

    checks: list[CheckResult] = []
    findings: list[Finding] = []
    for name, command in _selected_commands(args, ctx.settings):
        completed = await omp.env.sh.run(
            command,
            cwd=omp.env.info().root,
            timeout=omp.Duration(str(ctx.settings.get("timeout", "2m"))),
        )
        passed = completed.outcome is omp.env.Outcome.EXITED and completed.exit_code == 0
        summary = _bounded(
            completed.text().strip() or ("passed" if passed else "check failed without output"),
            _MAX_SUMMARY_BYTES,
        )
        checks.append(CheckResult(name, command, passed, completed.exit_code, summary))
        if not passed:
            findings.append(Finding(name, completed.exit_code, summary))
    return RulesCheckPayload(not findings, tuple(checks), tuple(findings))


def _clean_path(value: object) -> str | None:
    """Normalize one bounded workspace-relative path."""

    if not isinstance(value, str):
        return None
    path = value.strip().removeprefix("./")
    if (
        not path
        or path.startswith(("/", "artifact://", "http://", "https://"))
        or "\x00" in path
        or len(path.encode("utf-8")) > _MAX_PATH_BYTES
    ):
        return None
    return path


def _paths_in_patch(patch: object) -> tuple[str, ...]:
    """Extract hashline paths without parsing prose output."""

    if not isinstance(patch, str):
        return ()
    paths: list[str] = []
    for line in patch.splitlines():
        match = _PATCH_HEADER.fullmatch(line)
        if match is not None:
            paths.append(match.group(1))
    return tuple(paths)


def _touched_paths(event: omp.ToolResultEvent) -> tuple[str, ...]:
    """Read successful edit paths from structured target and payload fields."""

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
    return tuple(
        sorted({path for value in values if (path := _clean_path(value)) is not None})
    )[:_MAX_PATHS]


def _decision_globs(settings: Mapping[str, object]) -> tuple[str, ...]:
    """Return the declared decision-path globs."""

    raw = settings.get("decision_paths", _DEFAULT_DECISION_PATHS)
    if not isinstance(raw, Sequence) or isinstance(raw, (str, bytes)):
        raise ValueError("settings.decision_paths must be an array")
    globs = tuple(item for item in raw if isinstance(item, str) and item)
    if len(globs) != len(raw):
        raise ValueError("settings.decision_paths must contain non-empty strings")
    return globs


def _matching_paths(paths: Sequence[str], settings: Mapping[str, object]) -> tuple[str, ...]:
    """Filter paths through the configured decision-path declaration."""

    globs = _decision_globs(settings)
    return tuple(path for path in paths if any(fnmatch.fnmatchcase(path, glob) for glob in globs))


@omp.hook(
    "tool_result",
    phase=omp.HookPhase.OBSERVE,
    when=omp.When(
        target=frozenset({omp.TargetKind.CORE}),
        name=frozenset({"edit", "write"}),
    ),
)
async def remember_decision_paths(event: omp.ToolResultEvent, ctx: omp.Context) -> None:
    """Journal successful writes that match declared decision paths."""

    if event.outcome is not omp.OutcomeKind.OK or not isinstance(event.target, omp.CoreTool):
        return
    paths = _matching_paths(_touched_paths(event), ctx.settings)
    if paths:
        omp.journal.append(
            DecisionTouch(event.call_id, paths),
            idempotency_key=f"rules-sync-touch:{event.call_id}",
        )


def _pending_decision_paths() -> tuple[str, ...]:
    """Fold unconsumed decision touches from durable journal truth."""

    prior = omp.journal.latest(DecisionRecord)
    since = None if prior is None else prior.id
    paths = {
        path
        for row in omp.journal.entries(DecisionTouch, since=since)
        if isinstance(row.value, DecisionTouch)
        for path in row.value.paths
    }
    return tuple(sorted(paths))[:_MAX_PATHS]


@omp.hook("agent_settled")
async def capture_decision(
    event: omp.AgentSettledEvent, ctx: omp.Context
) -> omp.agents.Settle:
    """Append one ADR-shaped record when the submission touched decision paths."""

    del ctx
    paths = _pending_decision_paths()
    if paths:
        joined = ", ".join(paths)
        omp.journal.append(
            DecisionRecord(
                submission_id=event.submission_id,
                title="Record changes to declared decision paths",
                status="accepted",
                context=f"The settled submission changed: {joined}.",
                decision="Treat the settled changes as the repository's current recorded decision.",
                consequences="Future work must preserve or explicitly supersede these decision files.",
                paths=paths,
                settle_reason=event.reason.value,
            ),
            idempotency_key=f"rules-sync-decision:{event.submission_id}",
        )
    return omp.agents.Settle()
