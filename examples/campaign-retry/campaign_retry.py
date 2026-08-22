"""A durable bounded campaign that escalates across three settled turns."""

from __future__ import annotations

from dataclasses import dataclass

import omp


@dataclass(frozen=True, slots=True)
class RetryState:
    """Journaled state restored after extension-host replacement."""

    turns: int = 0


@omp.campaign(
    "three-turn-retry",
    at=omp.SETTLE,
    ladder=omp.Ladder(3),
    exhaust=omp.Exhaust.SETTLE,
    scope=omp.CampaignScope.SESSION,
    state=RetryState,
    state_family="examples.campaign-retry.state",
    on_failure=omp.OnFailure.DEFER,
)
def three_turn_retry(event: dict[str, object], state: RetryState) -> tuple[object, RetryState]:
    """Continue twice, then queue an exclusive tool force on the third turn."""

    next_state = RetryState(state.turns + 1)
    if bool(event.get("satisfied")):
        return omp.Done(), next_state
    if next_state.turns < 3:
        return omp.Continue(inject={"kind": "campaign-reminder"}), next_state
    return omp.Force("write", args={"content": "campaign complete"}), next_state
