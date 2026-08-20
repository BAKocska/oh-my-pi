from __future__ import annotations

import dataclasses
import json

import omp
from omp import Context, LiftedCall, Payload, RecordedCall, Rev
from omp.env import Edit, Format, OnStale

_REPLACE_REV = Rev.parse("rep.1")
_HASHLINE_REV = Rev.parse("hl.1")


@dataclasses.dataclass(frozen=True, slots=True)
class PatchEdit:
    """One resolved byte-range replacement in the patch dialect."""

    start: int
    end: int
    replacement: str


@dataclasses.dataclass(frozen=True, slots=True)
class PatchArgs:
    """Arguments for one document-authority patch."""

    path: str
    edits: list[PatchEdit]


@dataclasses.dataclass(frozen=True, slots=True)
class AppliedEdit:
    """Dialect-neutral facts needed to re-express a settled edit."""

    path: str
    start: int
    end: int
    replacement: str


@dataclasses.dataclass(frozen=True, slots=True)
class PatchApplied(Payload):
    """The durable result shared by replace, hashline, and patch families."""

    before: str
    after: str
    rebased: bool
    formatted: bool
    edits: list[AppliedEdit]


@dataclasses.dataclass(frozen=True, slots=True)
class PatchFault(omp.Fault):
    """A structured patch rejection that the model can correct."""

    kind: str
    detail: str


def _json_object(raw: bytes) -> dict[str, object] | None:
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError, TypeError):
        return None
    return value if isinstance(value, dict) else None


def _matches_source_schema(from_rev: Rev, raw_args: bytes) -> bool:
    source = _json_object(raw_args)
    if source is None:
        return False
    if from_rev == _REPLACE_REV:
        edits = source.get("edits")
        return isinstance(edits, list) and all(isinstance(edit, dict) for edit in edits)
    if from_rev == _HASHLINE_REV:
        return isinstance(source.get("input"), str)
    return False


def _resolved_patch(verdict: dict[str, object]) -> PatchArgs | None:
    payload = verdict.get("payload")
    if not isinstance(payload, dict):
        return None
    raw_edits = payload.get("edits")
    if not isinstance(raw_edits, list) or not raw_edits:
        return None

    path: str | None = None
    edits: list[PatchEdit] = []
    for raw_edit in raw_edits:
        if not isinstance(raw_edit, dict):
            return None
        candidate = raw_edit.get("path")
        start = raw_edit.get("start")
        end = raw_edit.get("end")
        replacement = raw_edit.get("replacement")
        if (
            not isinstance(candidate, str)
            or not candidate
            or isinstance(start, bool)
            or not isinstance(start, int)
            or isinstance(end, bool)
            or not isinstance(end, int)
            or not isinstance(replacement, str)
            or start < 0
            or end < start
        ):
            return None
        if path is not None and candidate != path:
            return None
        path = candidate
        edits.append(PatchEdit(start=start, end=end, replacement=replacement))

    assert path is not None
    return PatchArgs(path=path, edits=edits)


class _PatchProjection:
    @staticmethod
    def lift(from_rev: Rev, call: RecordedCall) -> LiftedCall | None:
        """Lift one rep.1 or hl.1 call directly into the live patch family."""
        if (
            call.identity.name != "edit"
            or call.identity.rev != from_rev
            or not _matches_source_schema(from_rev, call.raw_args)
        ):
            return None
        verdict = _json_object(call.verdict)
        if verdict is None:
            return None
        args = _resolved_patch(verdict)
        if args is None:
            return None
        return LiftedCall.of(args, verdict)


@omp.device("edit", family="patch", rev=1, place="env")
async def edit_patch(args: PatchArgs, ctx: Context) -> PatchApplied | PatchFault:
    """Commit resolved byte edits through one revision-pinned document lease."""
    del ctx
    if not args.path or not args.edits:
        return PatchFault(kind="invalid_patch", detail="path and edits must be non-empty")

    omp.env.require(omp.env.Capability.DOC_READ, omp.env.Capability.DOC_WRITE)
    async with await omp.env.docs.open(omp.EnvPath(args.path)) as doc:
        try:
            result = await doc.edit(
                [
                    Edit(
                        start=edit.start,
                        end=edit.end,
                        replacement=edit.replacement.encode("utf-8"),
                    )
                    for edit in args.edits
                ],
                on_stale=OnStale.REBASE,
                format=Format.BEST_EFFORT,
            )
        except omp.env.EnvError as error:
            return PatchFault(kind=type(error).__name__.lower(), detail=error.message)

    return PatchApplied(
        before=result.previous.hex,
        after=result.revision.hex,
        rebased=result.rebased,
        formatted=result.formatted,
        edits=[
            AppliedEdit(
                path=args.path,
                start=edit.start,
                end=edit.end,
                replacement=edit.replacement,
            )
            for edit in args.edits
        ],
    )


edit_patch.lift = _PatchProjection.lift
