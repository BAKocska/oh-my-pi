from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

import omp
from omp import Context, StateScope, entry_kind, tool
from omp.agents import ContinuationLedger, continuations, loop_signal


_GOAL_REGIME = "goal-loop"


@entry_kind("examples.goal_loop.goal", rev="v.1")
@dataclass(frozen=True, slots=True)
class GoalState:
    """Record the current session goal and whether it has been met."""

    objective: str
    met: bool


@dataclass(frozen=True, slots=True)
class GoalRegimeState:
    """Carry the objective used by the durable settle middleware."""

    objective: str


@dataclass(frozen=True, slots=True)
class GoalArgs:
    """Select a goal_set, goal_status, or goal_complete operation."""

    op: Literal["goal_set", "goal_status", "goal_complete"]
    objective: str | None = None


async def _current_goal() -> GoalState | None:
    record = await omp.state.latest(GoalState, scope=StateScope.SESSION)
    return None if record is None else record.value


def _ledger_view(ledger: ContinuationLedger) -> dict[str, int | str | None]:
    return {
        "consecutive": ledger.consecutive,
        "total": ledger.total,
        "cap": ledger.cap,
        "refusals": ledger.refusals,
        "owner": ledger.owner,
    }


async def _stop_goal_regimes() -> None:
    for activation in await omp.regimes.active():
        if activation.regime == _GOAL_REGIME:
            await omp.regimes.stop(activation.id)


@omp.regime(
    _GOAL_REGIME,
    on=omp.SETTLE,
    lifetime="session",
    state=GoalRegimeState,
    on_failure="defer",
)
async def continue_goal(ctx: omp.RegimeContext, next_: omp.Next) -> object:
    """Retry an unmet goal while accepting transient stalls."""

    state = ctx.state.value
    current = await _current_goal()
    if current is None or current.met or current.objective != state.objective:
        return next_.complete()

    signal = await loop_signal()
    if signal.stalled:
        return None

    ctx.context.append(
        "Continue working toward the registered objective. Only mark it complete "
        "after verifying the result.\n"
        f"<objective>{state.objective}</objective>"
    )
    return next_.retry()


@tool("goal", kind="soft", rev=1)
async def goal(args: GoalArgs, ctx: Context) -> dict[str, object]:
    """Register, inspect, or complete the autonomous goal for this session."""

    if args.op == "goal_set":
        objective = (args.objective or "").strip()
        if not objective:
            raise ValueError("goal_set requires a non-empty objective")
        state = GoalState(objective=objective, met=False)
        await omp.state.append(state, scope=StateScope.SESSION)
        await _stop_goal_regimes()
        await omp.regimes.start(
            _GOAL_REGIME,
            state=GoalRegimeState(objective=objective),
        )
    elif args.op == "goal_complete":
        current = await _current_goal()
        if current is None:
            return {"active": False, "met": False, "goal": None}
        state = GoalState(objective=current.objective, met=True)
        await omp.state.append(state, scope=StateScope.SESSION)
        await _stop_goal_regimes()
    else:
        state = await _current_goal()

    ledger = await continuations()
    return {
        "active": state is not None and not state.met,
        "met": state.met if state is not None else False,
        "goal": state.objective if state is not None else None,
        "continuations": _ledger_view(ledger),
    }
