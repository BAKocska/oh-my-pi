"""Optional typed journal observer for the feature-bundle example."""

from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import journal


@omp.entry_kind("examples.feature-bundle.stamp", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class TurnStamp:
    """Durable record that this bundle observed one settled turn."""

    session_id: str
    turn: int | None


@omp.hook("turn_end")
async def stamp_turn(payload: object, ctx: omp.Context) -> None:
    """Append one typed stamp after a turn has settled."""

    del payload
    journal.append(
        TurnStamp(session_id=ctx.session, turn=ctx.turn),
        idempotency_key=f"feature-bundle.stamp:{ctx.session}:{ctx.turn}",
    )
