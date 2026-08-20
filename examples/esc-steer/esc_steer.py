"""Abort the active turn before delivering one queued follow-up."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

import omp
from omp import ui


_SCOPE = omp.StateScope.SESSION


@omp.entry_kind(
    "examples.esc-steer.queue-transition", rev="v.1", display=False, spill=False
)
@dataclass(frozen=True, slots=True)
class QueueTransition:
    """Record one enqueue or successful FIFO delivery."""

    op: Literal["enqueue", "dequeue"]
    text: str = ""


def _fold_queue(queue: tuple[str, ...], record: object) -> tuple[str, ...]:
    """Apply one durable queue transition without inventing mutable state."""

    transition = getattr(record, "value", record)
    if not isinstance(transition, QueueTransition):
        return queue
    if transition.op == "enqueue":
        return (*queue, transition.text)
    return queue[1:] if queue else queue


async def _queued() -> tuple[str, ...]:
    """Rebuild this session's pending follow-ups in FIFO order."""

    queue, _watermark = await omp.state.fold(
        QueueTransition,
        _fold_queue,
        (),
        scope=_SCOPE,
    )
    return queue


@omp.command(
    "queue",
    description="Queue a follow-up for the next abort-and-steer shortcut",
    args=(ui.Arg("text", "Follow-up text", usage="<text ...>"),),
    hint="<text ...>",
)
async def queue_follow_up(
    invocation: ui.Invocation, ctx: omp.Context
) -> ui.Consumed:
    """Append one non-empty follow-up to this session's durable FIFO."""

    text = " ".join(invocation.argv).strip()
    if not text:
        return ui.Consumed(ui.text("Usage: /queue <text>"))
    await omp.state.append(QueueTransition("enqueue", text), scope=_SCOPE)
    pending = await _queued()
    return ui.Consumed(ui.text(f"Queued follow-up {len(pending)}: {text}"))


async def _abort_then_inject(next_item: str | None) -> None:
    """Request an immediate loop interrupt, then enqueue one follow-up."""

    await omp.agents.inject(
        "",
        mode=omp.agents.DeliveryMode.STEER,
        visible=False,
        role="system",
    )
    if next_item is not None:
        await omp.agents.inject(
            next_item,
            mode=omp.agents.DeliveryMode.NEXT_TURN,
            visible=True,
            role="user",
        )


@omp.shortcut(
    "ctrl+alt+escape",
    action_id="esc-steer.abort-next",
    description="Abort the running turn and continue with the next queued follow-up",
)
async def abort_and_steer(action: ui.Action, ctx: omp.Context) -> None:
    """Abort now and deliver at most one queued follow-up."""

    pending = await _queued()
    next_item = pending[0] if pending else None
    await _abort_then_inject(next_item)
    if next_item is not None:
        await omp.state.append(QueueTransition("dequeue"), scope=_SCOPE)
