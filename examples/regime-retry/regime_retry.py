"""A durable bounded regime that escalates across three settled turns."""

from __future__ import annotations

from dataclasses import dataclass

import omp


@dataclass(frozen=True, slots=True)
class RetryState:
    """Journaled state restored after extension-host replacement."""

    turns: int = 0


def _retry_limit(ctx: omp.RegimeContext, next_: omp.Next) -> object:
    """Complete the activation after its third committed step."""

    del ctx
    return next_.complete()


@omp.regime(
    "three-turn-retry",
    on=(omp.TOOL_CHOICE, omp.SETTLE),
    lifetime="session",
    state=RetryState,
    max_steps=3,
    on_limit=_retry_limit,
    on_failure="defer",
)
def three_turn_retry(ctx: omp.RegimeContext, next_: omp.Next) -> object | None:
    """Retry twice, then require the write tool and complete."""

    state = ctx.state.value
    if ctx.event.point is omp.TOOL_CHOICE:
        if state.turns >= 2:
            ctx.tool.require("write")
        return None

    next_state = RetryState(state.turns + 1)
    ctx.state.replace(next_state)
    if bool(getattr(ctx.event, "satisfied", False)):
        return next_.complete()
    if next_state.turns < 3:
        ctx.context.append({"kind": "regime-reminder"})
        return next_.retry()
    return next_.complete()
