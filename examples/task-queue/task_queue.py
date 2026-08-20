from __future__ import annotations

from dataclasses import dataclass, replace
from typing import Literal

import omp
from omp import ui
from omp.agents import Continue, ContinuationLedger, Settle, continuations


_TaskEvent = Literal["queued", "started", "done"]
_TaskStatus = Literal["queued", "started", "done"]


@omp.entry_kind(
    "examples.task-queue.queued-task", rev="v.1", display=False, spill=False
)
@dataclass(frozen=True, slots=True)
class QueuedTask:
    """Record one durable queue transition without projecting task text."""

    event: _TaskEvent
    task_id: int | None = None
    task: str | None = None


@dataclass(frozen=True, slots=True)
class QueueTaskArgs:
    """Describe one task to append to the serialized work queue."""

    task: str


@dataclass(frozen=True, slots=True)
class QueueReceipt:
    """Report the durable queue id without echoing hidden task text."""

    task_id: int
    waiting: int


@dataclass(frozen=True, slots=True)
class _QueueItem:
    task_id: int
    task: str
    status: _TaskStatus


def _fold_queue() -> list[_QueueItem]:
    tasks: dict[int, _QueueItem] = {}
    for row in omp.journal.entries(QueuedTask):
        transition = row.value
        if not isinstance(transition, QueuedTask):
            raise RuntimeError(f"queue entry {row.id} is not decodable")

        if transition.event == "queued":
            task = (transition.task or "").strip()
            if task:
                tasks[row.id.index] = _QueueItem(row.id.index, task, "queued")
        elif transition.task_id in tasks:
            current = tasks[transition.task_id]
            if transition.event == "started" and current.status == "queued":
                tasks[transition.task_id] = replace(current, status="started")
            elif transition.event == "done" and current.status == "started":
                tasks[transition.task_id] = replace(current, status="done")
    return list(tasks.values())


def _append_task(task: str, *, idempotency_key: str) -> QueueReceipt:
    text = task.strip()
    if not text:
        raise ValueError("task must not be empty")
    entry_id = omp.journal.append(
        QueuedTask(event="queued", task=text),
        idempotency_key=idempotency_key,
    )
    waiting = sum(item.status != "done" for item in _fold_queue())
    return QueueReceipt(task_id=entry_id.index, waiting=waiting)


@omp.device(
    "queue_task",
    family="queue",
    rev=1,
    place="host",
    summary="Append a task to the hidden, strictly serialized session queue.",
)
async def queue_task_device(args: QueueTaskArgs, ctx: omp.Context) -> QueueReceipt:
    """Queue work durably without exposing any waiting task to context."""

    return _append_task(
        args.task,
        idempotency_key=f"task-queue:device:{ctx.invocation}",
    )


@omp.command(
    "queue_task",
    description="Append work to the hidden serialized task queue",
    args=(ui.Arg("task", description="Task to run after earlier work settles"),),
    hint="<task>",
)
async def queue_task_command(
    invocation: ui.Invocation, ctx: omp.Context
) -> ui.Consumed:
    """Queue command text locally instead of submitting it as a prompt."""

    _append_task(
        invocation.raw,
        idempotency_key=f"task-queue:command:{ctx.invocation}",
    )
    return ui.Consumed()


@omp.hook("agent_settled")
async def run_next_task(
    event: omp.AgentSettledEvent, ctx: omp.Context
) -> Continue | Settle:
    """Complete the active item, then continue with at most one waiting task."""

    del ctx
    tasks = _fold_queue()
    active = next((item for item in tasks if item.status == "started"), None)
    if active is not None:
        omp.journal.append(
            QueuedTask(event="done", task_id=active.task_id),
            idempotency_key=(
                f"task-queue:settle:{event.submission_id}:done:{active.task_id}"
            ),
        )
        tasks = _fold_queue()

    next_task = next((item for item in tasks if item.status == "queued"), None)
    if next_task is None:
        return Settle()

    ledger: ContinuationLedger = await continuations()
    if ledger.consecutive >= ledger.cap:
        return Settle()

    omp.journal.append(
        QueuedTask(event="started", task_id=next_task.task_id),
        idempotency_key=(
            f"task-queue:settle:{event.submission_id}:start:{next_task.task_id}"
        ),
    )
    return Continue(
        prompt=next_task.task,
        visible=False,
        role="system",
        label="task-queue",
        collapse_prior=True,
    )
