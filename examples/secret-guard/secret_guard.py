"""Protect declared secret paths and delegate all value masking to Core."""

from __future__ import annotations

import fnmatch
import json
from enum import StrEnum
from pathlib import PurePosixPath
from typing import Any

import omp


_DEFAULT_PATHS = (
    ".env*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "id_rsa",
    "id_ed25519",
    "secret-placeholder.key",
)

_REDACTION_HITS = omp.telemetry.counter(
    "redaction_hits",
    unit="{call}",
    description="Tool calls whose unredacted policy input matched Core secret rules.",
)


class GuardAction(StrEnum):
    """Describe the secret-path action selected from canonical call facts."""

    DEFER = "defer"
    DENY = "deny"
    REVIEW = "review"


def _json_array(raw: object, *, setting: str) -> list[Any]:
    if isinstance(raw, str):
        value = json.loads(raw)
    else:
        value = raw
    if not isinstance(value, list):
        raise ValueError(f"{setting} must be a JSON array")
    return value


def protected_paths(settings: object) -> tuple[str, ...]:
    """Return the configured secret-path globs or the conservative defaults."""

    raw = settings.get("protected_paths") if hasattr(settings, "get") else None
    if raw in (None, ""):
        return _DEFAULT_PATHS
    values = _json_array(raw, setting="protected_paths")
    if not all(isinstance(value, str) and value for value in values):
        raise ValueError("protected_paths entries must be non-empty strings")
    return tuple(values)


def declare_secret_rules(settings: object) -> int:
    """Declare custom value rules while leaving built-in credential patterns to Core."""

    raw = settings.get("secret_rules", "[]") if hasattr(settings, "get") else "[]"
    entries = _json_array(raw, setting="secret_rules")
    for entry in entries:
        if not isinstance(entry, dict) or not isinstance(entry.get("pattern"), str):
            raise ValueError("secret_rules entries require a string pattern")
        replacement = entry.get("replacement")
        if replacement is not None and not isinstance(replacement, str):
            raise ValueError("secret rule replacement must be a string or null")
        omp.secrets.declare(
            omp.SecretRule(
                pattern=entry["pattern"],
                kind=omp.SecretKind(str(entry.get("kind", "literal"))),
                mode=omp.SecretMode(str(entry.get("mode", "obfuscate"))),
                label=str(entry.get("label", "")),
                replacement=replacement,
            )
        )
    return len(entries)


def _without_selector(path: str) -> str:
    if "://" in path:
        return path
    return path.partition(":")[0]


def _path_matches(path: str, patterns: tuple[str, ...]) -> bool:
    candidate = _without_selector(path).replace("\\", "/")
    name = PurePosixPath(candidate).name
    return any(
        fnmatch.fnmatchcase(candidate, pattern) or fnmatch.fnmatchcase(name, pattern)
        for pattern in patterns
    )


def assess_bash(ir: omp.BashIR, patterns: tuple[str, ...]) -> GuardAction:
    """Deny proven secret reads and send unresolved read effects to approval."""

    if not ir.parse_ok or ir.truncated:
        return GuardAction.REVIEW

    dynamic_read = False
    for ref in ir.reads:
        if ref.dynamic:
            dynamic_read = True
            continue
        if any(
            candidate is not None and _path_matches(candidate, patterns)
            for candidate in (ref.lexical, ref.resolved, ref.absolute)
        ):
            return GuardAction.DENY

    if dynamic_read or ir.has_dynamic_eval:
        return GuardAction.REVIEW
    return GuardAction.DEFER


def assess_call(event: omp.ToolCallEvent, patterns: tuple[str, ...]) -> GuardAction:
    """Classify shell path facts and canonical Core read-tool targets."""

    if event.bash is not None:
        return assess_bash(event.bash, patterns)
    if isinstance(event.target, omp.CoreTool) and event.target.name == "read":
        path = event.target.args.get("path")
        if isinstance(path, str) and _path_matches(path, patterns):
            return GuardAction.DENY
    return GuardAction.DEFER


def _approval(event: omp.ToolCallEvent) -> omp.RequireApproval:
    return omp.RequireApproval(
        omp.ApprovalSpec(
            title="Review unresolved secret-path read",
            body="The shell analysis found a dynamic or incomplete read target.",
            subject=f"tool call {event.call_id}",
            kind=omp.ApprovalKind.READ,
            scopes=(omp.PolicyScope.ONCE,),
            evidence=("Dynamic paths are not treated as a static allow.",),
        )
    )


@omp.hook("extension_activate", phase=omp.HookPhase.OBSERVE, order=0)
async def activate(payload: object, ctx: omp.Context) -> None:
    """Install extension-authored secret rules beside Core's built-ins."""

    del payload
    declare_secret_rules(ctx.settings)


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.PRECHECK,
    order=-100,
    on_failure=omp.OnFailure.DENY,
)
async def deny_secret_reads(
    event: omp.ToolCallEvent, ctx: omp.Context
) -> omp.HookDecision:
    """Deny only statically proven reads of configured secret paths."""

    action = assess_call(event, protected_paths(ctx.settings))
    if action is GuardAction.DENY:
        return omp.Deny(
            "Reading a declared secret path is blocked.",
            code="secret_guard.protected_read",
        )
    return omp.Defer()


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.APPROVAL,
    order=-100,
    on_failure=omp.OnFailure.DENY,
)
async def review_dynamic_reads(
    event: omp.ToolCallEvent, ctx: omp.Context
) -> omp.HookDecision:
    """File a durable approval ticket for unresolved shell read effects."""

    action = assess_call(event, protected_paths(ctx.settings))
    if action is GuardAction.REVIEW:
        return _approval(event)
    return omp.Defer()


@omp.hook("tool_call", phase=omp.HookPhase.OBSERVE, order=100)
async def count_redaction_hits(event: omp.ToolCallEvent, ctx: omp.Context) -> None:
    """Count calls for which Core's masker changes the unredacted policy input."""

    del ctx
    surface = "bash"
    text: str | None = event.bash.source if event.bash is not None else None
    if text is None and isinstance(event.target, omp.CoreTool) and event.target.name == "read":
        path = event.target.args.get("path")
        text = path if isinstance(path, str) else None
        surface = "read"
    if text is not None and omp.secrets.mask(text) != text:
        _REDACTION_HITS.add(1, surface=surface)
