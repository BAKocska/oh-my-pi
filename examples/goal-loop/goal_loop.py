from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

import omp
from omp import StateScope
from omp import entry_kind
from omp import Context, tool
from omp import AgentSettledEvent, hook
from omp.agents import (
    Continue,
    ContinuationLedger,
    LoopSignal,
    Settle,
    continuations,
    loop_signal,
)


@entry_kind("examples.goal_loop.goal", rev="v.1")
@dataclass(frozen=True, slots=True)
class GoalState:
    """Record the current session goal and whether it has been met."""

    objective: str
    met: bool


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


@tool("goal", kind="soft", rev=1)
async def goal(args: GoalArgs, ctx: Context) -> dict[str, object]:
    """Register, inspect, or complete the autonomous goal for this session."""

    if args.op == "goal_set":
        objective = (args.objective or "").strip()
        if not objective:
            raise ValueError("goal_set requires a non-empty objective")
        state = GoalState(objective=objective, met=False)
        await omp.state.append(state, scope=StateScope.SESSION)
    elif args.op == "goal_complete":
        current = await _current_goal()
        if current is None:
            return {"active": False, "met": False, "goal": None}
        state = GoalState(objective=current.objective, met=True)
        await omp.state.append(state, scope=StateScope.SESSION)
    else:
        state = await _current_goal()

    ledger = await continuations()
    return {
        "active": state is not None and not state.met,
        "met": state.met if state is not None else False,
        "goal": state.objective if state is not None else None,
        "continuations": _ledger_view(ledger),
    }


@hook("agent_settled")
async def continue_goal(
    event: AgentSettledEvent, ctx: Context
) -> Continue | Settle:
    """Continue an unmet goal unless Core reports that the loop is stalled."""

    goal_state = await _current_goal()
    if goal_state is None or goal_state.met:
        return Settle()

    signal: LoopSignal = await loop_signal()
    if signal.stalled:
        return Settle()

    return Continue(
        prompt=(
            "Continue working toward the registered objective. Only mark it complete "
            "after verifying the result.\n"
            f"<objective>{goal_state.objective}</objective>"
        ),
        label="goal-loop",
        collapse_prior=True,
    )
