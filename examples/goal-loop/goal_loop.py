from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

import omp
from omp import Context, StateScope, entry_kind, tool
from omp.agents import ContinuationLedger, continuations, loop_signal


_GOAL_CAMPAIGN = "goal-loop"


@entry_kind("examples.goal_loop.goal", rev="v.1")
@dataclass(frozen=True, slots=True)
class GoalState:
    """Record the current session goal and whether it has been met."""

    objective: str
    met: bool


@dataclass(frozen=True, slots=True)
class GoalCampaignState:
    """Carry the goal fields used by the durable settle decision."""

    objective: str
    met: bool = False


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


async def _disengage_goal_campaigns() -> None:
    for engagement in await omp.campaigns.active():
        if engagement.campaign == _GOAL_CAMPAIGN:
            await omp.campaigns.disengage(engagement.id)


@omp.campaign(
    _GOAL_CAMPAIGN,
    at=omp.SETTLE,
    exhaust=omp.Exhaust.SETTLE,
    scope=omp.CampaignScope.SESSION,
    state=GoalCampaignState,
    state_family="examples.goal-loop.state",
    on_failure=omp.OnFailure.DEFER,
)
async def continue_goal(
    event: dict[str, object], state: GoalCampaignState
) -> tuple[object, GoalCampaignState]:
    """Continue an unmet goal and finish when it completes or progress stalls."""

    current = await _current_goal()
    if (
        state.met
        or current is None
        or current.met
        or current.objective != state.objective
    ):
        return omp.Done(), state

    signal = await loop_signal()
    if signal.stalled:
        return omp.Done(), state

    return (
        omp.Continue(
            inject=(
                "Continue working toward the registered objective. Only mark it complete "
                "after verifying the result.\n"
                f"<objective>{state.objective}</objective>"
            )
        ),
        state,
    )


@tool("goal", kind="soft", rev=1)
async def goal(args: GoalArgs, ctx: Context) -> dict[str, object]:
    """Register, inspect, or complete the autonomous goal for this session."""

    if args.op == "goal_set":
        objective = (args.objective or "").strip()
        if not objective:
            raise ValueError("goal_set requires a non-empty objective")
        state = GoalState(objective=objective, met=False)
        await omp.state.append(state, scope=StateScope.SESSION)
        await _disengage_goal_campaigns()
        await omp.campaigns.engage(
            _GOAL_CAMPAIGN,
            state=GoalCampaignState(objective=objective),
        )
    elif args.op == "goal_complete":
        current = await _current_goal()
        if current is None:
            return {"active": False, "met": False, "goal": None}
        state = GoalState(objective=current.objective, met=True)
        await omp.state.append(state, scope=StateScope.SESSION)
        await _disengage_goal_campaigns()
    else:
        state = await _current_goal()

    ledger = await continuations()
    return {
        "active": state is not None and not state.met,
        "met": state.met if state is not None else False,
        "goal": state.objective if state is not None else None,
        "continuations": _ledger_view(ledger),
    }
