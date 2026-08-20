"""A journal-backed, artifact-gated specification workflow."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal
import json

import omp

Phase = Literal["brainstorm", "plan", "apply", "archived"]

_PHASE_ORDER: tuple[Phase, ...] = ("brainstorm", "plan", "apply", "archived")
_PHASE_ROLE: dict[Phase, str] = {
    "brainstorm": "brainstormer",
    "plan": "planner",
    "apply": "builder",
    "archived": "archivist",
}
_PHASE_DEVICES: dict[Phase, tuple[str, ...]] = {
    "brainstorm": ("flow", "flow/advance", "flow/status"),
    "plan": ("flow", "flow/advance", "flow/status"),
    "apply": ("flow", "flow/archive", "flow/status"),
    "archived": ("flow", "flow/start", "flow/status"),
}
_DEVICE_UNIVERSE = tuple(dict.fromkeys(path for paths in _PHASE_DEVICES.values() for path in paths))


@omp.entry_kind("examples.spec-flow.transition", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class FlowTransition:
    """Record one durable phase transition and its model-facing declaration."""

    ticket: str
    title: str
    phase: Phase
    role: str
    devices: tuple[str, ...]
    artifact_path: str | None


@omp.entry_kind("examples.spec-flow.archive", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class FlowArchive:
    """Keep the archived spec bundle reachable from session truth."""

    ticket: str
    artifact_paths: tuple[str, ...]
    bundle: omp.BlobRef


@dataclass(frozen=True, slots=True)
class StartArgs:
    """Name the coordinated ticket and specification."""

    ticket: str
    title: str


@dataclass(frozen=True, slots=True)
class AdvanceArgs:
    """Supply the completed current-phase document before advancing."""

    artifact: str | None = None


@dataclass(frozen=True, slots=True)
class ArchiveArgs:
    """Supply the completed apply document before archiving the bundle."""

    artifact: str | None = None


@dataclass(frozen=True, slots=True)
class StatusArgs:
    """Request the current journal-derived workflow status."""


@dataclass(frozen=True, slots=True)
class FlowPayload(omp.Payload):
    """Describe the current phase, role, devices, and durable artifacts."""

    active: bool
    ticket: str | None
    title: str | None
    phase: Phase | None
    role: str | None
    devices: tuple[str, ...]
    artifact_paths: tuple[str, ...]
    bundle: omp.BlobRef | None = None


@dataclass(frozen=True, slots=True)
class PhaseOrderFault(omp.Fault):
    """Refuse a phase operation that is invalid for the journal-derived state."""

    operation: str
    current_phase: Phase | None
    expected_phase: Phase | None


@dataclass(frozen=True, slots=True)
class MissingArtifactFault(omp.Fault):
    """Name the exact phase document required before a transition."""

    phase: Phase
    artifact_path: str
    ticket: str


def _transitions() -> tuple[FlowTransition, ...]:
    """Decode live transition entries in journal order."""

    return tuple(
        entry.value
        for entry in omp.journal.entries(FlowTransition)
        if isinstance(entry.value, FlowTransition)
    )


def _current() -> FlowTransition | None:
    """Derive the current phase exclusively from the session journal."""

    rows = _transitions()
    return rows[-1] if rows else None


def _artifact_path(ticket: str, phase: Phase) -> str:
    """Return the stable workspace path for one phase artifact."""

    return f".omp/specs/{ticket}/{phase}.md"


def _artifact_paths(ticket: str, through: Phase) -> tuple[str, ...]:
    """List the artifact paths through a completed phase."""

    end = _PHASE_ORDER.index(through)
    return tuple(_artifact_path(ticket, phase) for phase in _PHASE_ORDER[: end + 1] if phase != "archived")


async def _write_artifact(path: str, content: str) -> None:
    """Commit an artifact through a revision-pinned Environment document lease."""

    async with await omp.env.docs.open(omp.EnvPath(path), language="markdown", create=True) as doc:
        await doc.write(content)


async def _read_artifact(path: str) -> str:
    """Read an artifact through its Environment document lease."""

    async with await omp.env.docs.open(omp.EnvPath(path), language="markdown") as doc:
        return await doc.read()


async def _set_phase_availability(phase: Phase) -> None:
    """Publish one atomic availability delta without changing the tool array."""

    desired = frozenset(_PHASE_DEVICES[phase])
    await omp.devices.set_availability(
        *(
            omp.AvailabilityDelta(
                path=path,
                mounted=path in desired,
                reason=None if path in desired else f"unavailable during {phase}",
            )
            for path in _DEVICE_UNIVERSE
        )
    )


def _payload(current: FlowTransition | None, bundle: omp.BlobRef | None = None) -> FlowPayload:
    """Project journal truth as a structured device payload."""

    if current is None:
        return FlowPayload(False, None, None, None, None, (), (), bundle)
    completed: Phase = "apply" if current.phase == "archived" else _PHASE_ORDER[max(0, _PHASE_ORDER.index(current.phase) - 1)]
    paths = () if current.phase == "brainstorm" else _artifact_paths(current.ticket, completed)
    return FlowPayload(
        True,
        current.ticket,
        current.title,
        current.phase,
        current.role,
        current.devices,
        paths,
        bundle,
    )


@omp.device("flow", family="spec", rev=1, place="host")
async def flow(args: StatusArgs, ctx: omp.Context) -> FlowPayload:
    """Report the current spec workflow from typed journal entries."""

    del args, ctx
    return _payload(_current())


@flow.subtool("start")
async def start(args: StartArgs, ctx: omp.Context) -> FlowPayload | PhaseOrderFault:
    """Start a brainstorm phase for one durable ticket identity."""

    del ctx
    current = _current()
    if current is not None and current.phase != "archived":
        return PhaseOrderFault("start", current.phase, None)
    ticket = args.ticket.strip()
    title = args.title.strip()
    if not ticket or not title:
        return PhaseOrderFault("start", current.phase if current else None, None)
    transition = FlowTransition(
        ticket=ticket,
        title=title,
        phase="brainstorm",
        role=_PHASE_ROLE["brainstorm"],
        devices=_PHASE_DEVICES["brainstorm"],
        artifact_path=None,
    )
    omp.journal.append(transition, idempotency_key=f"spec-flow:start:{ticket}")
    await _set_phase_availability("brainstorm")
    return _payload(transition)


@flow.subtool("advance")
async def advance(
    args: AdvanceArgs, ctx: omp.Context
) -> FlowPayload | PhaseOrderFault | MissingArtifactFault:
    """Write the current artifact and advance brainstorm to plan or plan to apply."""

    del ctx
    current = _current()
    if current is None or current.phase not in {"brainstorm", "plan"}:
        return PhaseOrderFault("advance", current.phase if current else None, "brainstorm")
    path = _artifact_path(current.ticket, current.phase)
    artifact = (args.artifact or "").strip()
    if not artifact:
        return MissingArtifactFault(current.phase, path, current.ticket)
    await _write_artifact(path, artifact)
    next_phase: Phase = "plan" if current.phase == "brainstorm" else "apply"
    transition = FlowTransition(
        ticket=current.ticket,
        title=current.title,
        phase=next_phase,
        role=_PHASE_ROLE[next_phase],
        devices=_PHASE_DEVICES[next_phase],
        artifact_path=path,
    )
    omp.journal.append(
        transition,
        idempotency_key=f"spec-flow:{current.ticket}:{current.phase}:{next_phase}",
    )
    await _set_phase_availability(next_phase)
    return _payload(transition)


@flow.subtool("status")
async def status(args: StatusArgs, ctx: omp.Context) -> FlowPayload:
    """Return the current journal-derived phase without mutating it."""

    del args, ctx
    return _payload(_current())


@flow.subtool("archive")
async def archive(
    args: ArchiveArgs, ctx: omp.Context
) -> FlowPayload | PhaseOrderFault | MissingArtifactFault:
    """Write the apply artifact, spill the complete bundle, and journal its reference."""

    del ctx
    current = _current()
    if current is None or current.phase != "apply":
        return PhaseOrderFault("archive", current.phase if current else None, "apply")
    path = _artifact_path(current.ticket, "apply")
    artifact = (args.artifact or "").strip()
    if not artifact:
        return MissingArtifactFault("apply", path, current.ticket)
    await _write_artifact(path, artifact)
    paths = _artifact_paths(current.ticket, "apply")
    documents = tuple([await _read_artifact(item) for item in paths])
    encoded = json.dumps(
        {
            "ticket": current.ticket,
            "title": current.title,
            "artifacts": [
                {"path": item, "content": content}
                for item, content in zip(paths, documents, strict=True)
            ],
        },
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")
    bundle = await omp.env.blobs.put(encoded)
    transition = FlowTransition(
        ticket=current.ticket,
        title=current.title,
        phase="archived",
        role=_PHASE_ROLE["archived"],
        devices=_PHASE_DEVICES["archived"],
        artifact_path=path,
    )
    await omp.journal.append_atomic(
        (FlowArchive(current.ticket, paths, bundle), transition),
        idempotency_key=f"spec-flow:archive:{current.ticket}",
    )
    await _set_phase_availability("archived")
    return _payload(transition, bundle)
