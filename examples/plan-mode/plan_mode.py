"""A session-scoped read-only planning mode with typed plan handoff."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

import omp
from omp import BashIR  # GAP: not exported by frozen layer (docs/py/06-policy.md §4)
from omp import ModelRef  # GAP: not exported by frozen layer (docs/py/05-hooks.md §3.3)

_SESSION = omp.StateScope.SESSION
_DENIAL_CODE = "plan_readonly"


@omp.entry_kind("examples.plan-mode.transition", rev="v.1", display=False)
@dataclass(frozen=True, slots=True)
class PlanModeTransition:
    """Record one session-local transition into or out of planning mode."""

    op: Literal["enter", "exit"]
    model: ModelRef | None = None
    thinking: str | None = None


@omp.entry_kind("examples.plan-mode.plan", rev="v.1")
@dataclass(frozen=True, slots=True)
class Plan:
    """Record the completed plan handed from planning into execution."""

    text: str
    model: ModelRef
    thinking: str


@dataclass(frozen=True, slots=True)
class PlanArgs:
    """Select enter, exit, or status and carry planning configuration or text."""

    op: Literal["enter", "exit", "status"]
    model: ModelRef | None = None
    thinking: str | None = None
    plan: str | None = None


@dataclass(frozen=True, slots=True)
class _Mode:
    active: bool = False
    model: ModelRef | None = None
    thinking: str | None = None


def _fold_mode(current: _Mode, record: object) -> _Mode:
    transition: PlanModeTransition = record.value  # type: ignore[attr-defined]
    if transition.op == "exit":
        return _Mode()
    return _Mode(active=True, model=transition.model, thinking=transition.thinking)


async def _mode() -> _Mode:
    value, _watermark = await omp.state.fold(
        PlanModeTransition,
        _fold_mode,
        _Mode(),
        scope=_SESSION,
    )
    return value


def _status(mode: _Mode) -> dict[str, object]:
    return {
        "mode": "plan" if mode.active else "execute",
        "model": mode.model,
        "thinking": mode.thinking,
    }


@omp.tool("plan", kind="soft", rev=1)
async def plan(args: PlanArgs, ctx: omp.Context) -> dict[str, object]:
    """Enter planning, publish its completed plan on exit, or report mode status."""

    del ctx
    current = await _mode()
    if args.op == "status":
        return _status(current)

    if args.op == "enter":
        model = args.model
        thinking = (args.thinking or "").strip()
        if model is None or not thinking:
            raise ValueError("enter requires model and non-empty thinking selections")
        await omp.state.append(
            PlanModeTransition(op="enter", model=model, thinking=thinking),
            scope=_SESSION,
        )
        return _status(_Mode(active=True, model=model, thinking=thinking))

    if not current.active or current.model is None or current.thinking is None:
        raise ValueError("plan mode is not active")
    text = (args.plan or "").strip()
    if not text:
        raise ValueError("exit requires the completed plan")
    await omp.state.append(
        Plan(text=text, model=current.model, thinking=current.thinking),
        scope=_SESSION,
    )
    await omp.state.append(PlanModeTransition(op="exit"), scope=_SESSION)
    result = _status(_Mode())
    result["plan"] = text
    return result


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.PRECHECK,
    order=50,
    on_failure=omp.OnFailure.DENY,
)
async def deny_writes_while_planning(
    event: omp.ToolCallEvent, ctx: omp.Context
) -> omp.HookDecision:
    """Deny core write, edit, and non-read-only bash calls during planning."""

    del ctx
    if not (await _mode()).active:
        return omp.Defer()
    if not isinstance(event.target, omp.CoreTool):
        return omp.Defer()
    if event.target.name in {"write", "edit"}:
        return omp.Deny(
            "plan mode may not write to the filesystem",
            code=_DENIAL_CODE,
        )
    ir: BashIR | None = event.bash
    if event.target.name == "bash" and ir is not None and not ir.is_read_only():
        return omp.Deny(
            "plan mode may only run read-only shell commands",
            code=_DENIAL_CODE,
        )
    return omp.Defer()


@omp.hook("turn_start", phase=omp.HookPhase.TRANSFORM, order=50)
async def use_plan_inference(
    event: omp.TurnStartEvent, ctx: omp.Context
) -> omp.HookDecision:
    """Switch planning turns to their selected model and thinking level only."""

    del event, ctx
    mode = await _mode()
    if not mode.active:
        return omp.Defer()
    return omp.Modify(
        patch={"model": mode.model, "thinking": mode.thinking},
        reason="plan mode inference selection",
    )
