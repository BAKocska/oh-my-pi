"""Journal-backed workspace checkpoints and coordinated rewinds."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

import omp
from omp import Context, Payload, entry_kind, journal
from omp.agents import (  # GAP: not in frozen layer (docs/py/12-agents.md § Time travel)
    RestoreScope,
    RewindPending,
    rewind as agent_rewind,
    snapshot,
)


_RewindScope = Literal["workspace", "conversation", "both"]


@entry_kind("examples.rewind.checkpoint", rev="v.1", spill=False)
@dataclass(frozen=True, slots=True)
class Checkpoint:
    """Pair a content-addressed workspace generation with a transcript event."""

    generation: str
    event_index: int


@entry_kind("examples.rewind.background-warning", rev="v.1", spill=False)
@dataclass(frozen=True, slots=True)
class BackgroundChildrenWarning:
    """Record that a rewind deliberately left background children running."""

    target_event: int | None
    scope: _RewindScope
    message: str


@dataclass(frozen=True, slots=True)
class CheckpointArgs:
    """Arguments identifying the transcript event paired with a checkpoint."""

    event_index: int
    label: str | None = None


@dataclass(frozen=True, slots=True)
class CheckpointPayload(Payload):
    """The captured workspace generation and paired transcript event."""

    generation: str
    event_index: int


@dataclass(frozen=True, slots=True)
class RewindArgs:
    """Arguments selecting a transcript target, restore scope, and preview mode."""

    to: int | None
    scope: _RewindScope = "both"
    dry_run: bool = True


@dataclass(frozen=True, slots=True)
class RewindConflict:
    """One structured workspace conflict returned by a rewind preview."""

    path: str
    reason: str


@dataclass(frozen=True, slots=True)
class RewindPayload(Payload):
    """A structured preview or completed rewind report."""

    head: int
    dropped_items: int
    scope: _RewindScope
    dry_run: bool
    written: int
    deleted: int
    conflicts: tuple[RewindConflict, ...]
    undo_generation: str | None


@dataclass(frozen=True, slots=True)
class RewindFault(omp.Fault):
    """A checkpoint or rewind request rejected without a partial substitute."""

    detail: str


def _checkpoint_generation(to: int) -> str | None:
    """Find the newest live checkpoint at or before a transcript event."""

    generation: str | None = None
    checkpoint_event = -1
    for entry in journal.entries(Checkpoint):
        value = entry.value
        if (
            isinstance(value, Checkpoint)
            and checkpoint_event < value.event_index <= to
        ):
            generation = value.generation
            checkpoint_event = value.event_index
    return generation


def _payload(report, scope: _RewindScope) -> RewindPayload:
    """Project the host report without flattening structured conflicts into prose."""

    restore = report.restore
    return RewindPayload(
        head=report.head,
        dropped_items=report.dropped_items,
        scope=scope,
        dry_run=report.dry_run,
        written=restore.written if restore else 0,
        deleted=restore.deleted if restore else 0,
        conflicts=(
            tuple(
                RewindConflict(path=str(conflict.path), reason=conflict.reason)
                for conflict in restore.conflicts
            )
            if restore
            else ()
        ),
        undo_generation=restore.undo_snapshot_id if restore else None,
    )


@omp.tool("checkpoint", kind="soft")
async def checkpoint(args: CheckpointArgs, ctx: Context) -> CheckpointPayload | RewindFault:
    """Snapshot the workspace and journal its generation beside a transcript event."""

    del ctx
    if args.event_index < 0:
        return RewindFault("event_index must be non-negative")
    if not omp.env.has(omp.env.Capability.WORKSPACE_SNAPSHOT):
        return RewindFault("workspace snapshots are unavailable in this environment")

    workspace = await snapshot(label=args.label or f"event {args.event_index}")
    # Snapshot ids are blob-manifest hashes: the content-addressed workspace generation.
    value = Checkpoint(generation=workspace.id, event_index=args.event_index)
    journal.append(
        value,
        idempotency_key=f"checkpoint:{args.event_index}:{workspace.id}",
    )
    return CheckpointPayload(value.generation, value.event_index)


@omp.tool("rewind", kind="soft")
async def rewind(args: RewindArgs, ctx: Context) -> RewindPayload | RewindFault:
    """Preview or restore the conversation, workspace, or both at one event."""

    del ctx
    if args.to is not None and args.to < 0:
        return RewindFault("to must be a non-negative event index or null")

    wants_workspace = args.scope in {"workspace", "both"}
    generation: str | None = None
    if wants_workspace:
        if args.to is None:
            return RewindFault("workspace rewind requires a checkpointed event")
        generation = _checkpoint_generation(args.to)
        if generation is None:
            return RewindFault(f"no workspace checkpoint exists at or before event {args.to}")

    host_scope = {
        "conversation": RestoreScope.THREAD,
        "workspace": RestoreScope.WORKSPACE,
        "both": RestoreScope.BOTH,
    }[args.scope]

    try:
        # Every apply is preceded by the same real dry-run report exposed to callers.
        preview = await agent_rewind(
            args.to,
            scope=host_scope,
            snapshot_id=generation,
            dry_run=True,
        )
        if args.dry_run:
            return _payload(preview, args.scope)

        report = await agent_rewind(
            args.to,
            scope=host_scope,
            snapshot_id=generation,
            dry_run=False,
        )
    except RewindPending as error:
        return RewindFault(f"rewind is pending terminal turn receipt: {error}")

    if wants_workspace:
        # Load-bearing invariant: agents.rewind captures the current workspace before
        # restore. Never accept a workspace report without its undo generation.
        if report.restore is None or not report.restore.undo_snapshot_id:
            raise RuntimeError("workspace restore returned no pre-restore undo snapshot")

    journal.append(
        BackgroundChildrenWarning(
            target_event=args.to,
            scope=args.scope,
            message="Background children remain running and may settle after this rewind.",
        )
    )
    return _payload(report, args.scope)
