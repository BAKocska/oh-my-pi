"""Guard shell writes using host-provided BashIR path facts."""

from __future__ import annotations

from enum import StrEnum
from pathlib import PurePosixPath
from typing import Iterable

import omp


class SeatbeltTier(StrEnum):
    """Select the shell-write policy configured for this extension."""

    YOLO = "yolo"
    WRITE = "write"
    ASK = "ask"


def _configured_tier(ctx: omp.Context) -> SeatbeltTier:
    return SeatbeltTier(str(ctx.settings.get("tier", SeatbeltTier.YOLO)))


def _targets_git(path: omp.PathRef) -> bool:
    values = (path.lexical, path.resolved, path.absolute)
    return any(
        ".git" in PurePosixPath(value).parts
        for value in values
        if value is not None
    )


def _git_writes(ir: omp.BashIR) -> tuple[omp.PathRef, ...]:
    return tuple(path for path in ir.writes if _targets_git(path))


def precheck_decision(
    ir: omp.BashIR,
    roots: Iterable[omp.WorkspaceUri | str],
    tier: SeatbeltTier,
) -> omp.HookDecision:
    """Return the deny-only PRECHECK decision for one analyzed shell call."""
    git_writes = _git_writes(ir)
    if git_writes:
        targets = ", ".join(path.resolved or path.lexical for path in git_writes)
        return omp.Deny(
            f"yolo-seatbelt forbids mutation of Git metadata: {targets}",
            code="seatbelt.git-write",
        )

    if ir.is_read_only():
        return omp.Defer(note="read-only shell call")

    if tier is SeatbeltTier.YOLO:
        note = (
            "yolo tier cannot bound dynamic evaluation"
            if ir.has_dynamic_eval
            else "yolo tier permits non-.git effects"
        )
        return omp.Defer(note=note)

    if tier is SeatbeltTier.WRITE:
        outside = ir.writes_outside(roots)
        if outside:
            targets = ", ".join(path.resolved or path.lexical for path in outside)
            return omp.Deny(
                f"write tier confines mutations to workspace roots: {targets}",
                code="seatbelt.outside-write",
            )
        if ir.has_dynamic_eval:
            return omp.Deny(
                "write tier cannot bound dynamic shell evaluation",
                code="seatbelt.dynamic-eval",
            )

    return omp.Defer()


def approval_decision(
    ir: omp.BashIR,
    roots: Iterable[omp.WorkspaceUri | str],
    tier: SeatbeltTier,
) -> omp.HookDecision:
    """Return the durable APPROVAL decision for an ask-tier shell call."""
    if tier is not SeatbeltTier.ASK or ir.is_read_only() or _git_writes(ir):
        return omp.Defer()

    outside = ir.writes_outside(roots)
    evidence = ["tier=ask"]
    if outside:
        evidence.append("writes_outside=" + ",".join(path.lexical for path in outside))
    if ir.has_dynamic_eval:
        evidence.append("dynamic_eval")
    return omp.RequireApproval(
        omp.ApprovalSpec(
            title="Approve shell side effects",
            body="The ask tier requires approval for every non-read-only shell call.",
            subject=ir.source,
            kind=omp.ApprovalKind.EXEC,
            evidence=tuple(evidence),
        )
    )


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.PRECHECK,
    on_failure=omp.OnFailure.DENY,
)
async def enforce_seatbelt(
    payload: omp.ToolCallEvent,
    ctx: omp.Context,
) -> omp.HookDecision:
    """Deny only writes forbidden by the configured seatbelt tier."""
    if payload.bash is None:
        return omp.Defer()
    return precheck_decision(payload.bash, ctx.roots, _configured_tier(ctx))


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.APPROVAL,
    on_failure=omp.OnFailure.DENY,
)
async def request_shell_approval(
    payload: omp.ToolCallEvent,
    ctx: omp.Context,
) -> omp.HookDecision:
    """File an ask-tier approval ticket without awaiting user interaction."""
    if payload.bash is None:
        return omp.Defer()
    return approval_decision(payload.bash, ctx.roots, _configured_tier(ctx))
