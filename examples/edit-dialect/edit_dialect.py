from __future__ import annotations

import dataclasses
import json

import omp
from omp import Context, Faulted, LiftedCall, Ok, Part, Payload, PromptCaps, RecordedCall, Rev
from omp.env import Format, OnStale  # GAP: not in frozen layer yet (docs/py/11 §Document leases)

_DIALECT_DOCS = """HLX edit dialect
Pass one `patch` string containing hashline sections. Each section starts `[PATH#TAG]`; use `PUT`, `CUT`, `REM`, or `MV`, and prefix every replacement-body line with `+`. A call may mention only one document path so its mutation stays on one revision-pinned lease.
"""


@dataclasses.dataclass(slots=True)
class EditArgs:
    """Arguments for the HLX edit dialect."""

    patch: str


@dataclasses.dataclass(frozen=True, slots=True)
class EditApplied(Payload):
    """Dialect-neutral facts about a committed document edit."""

    before: str
    after: str
    rebased: bool
    formatted: bool
    changed: list[str]


@dataclasses.dataclass(frozen=True, slots=True)
class EditFault(omp.Fault):
    """A structured document rejection that the model can correct."""

    kind: str
    detail: str
    expected: str | None = None
    current: str | None = None
    ranges: list[str] = dataclasses.field(default_factory=list)


def _single_document_path(patch: str) -> str | None:
    path: str | None = None
    for line in patch.splitlines():
        if not (line.startswith("[") and line.endswith("]") and "#" in line):
            continue
        candidate, _tag = line[1:-1].rsplit("#", 1)
        if not candidate or (path is not None and candidate != path):
            return None
        path = candidate
    return path


def _bounded(text: str, maximum_bytes: int) -> str:
    encoded = text.encode("utf-8")
    if len(encoded) <= maximum_bytes:
        return text
    return encoded[:maximum_bytes].decode("utf-8", errors="ignore")


class _DialectProjection:
    @staticmethod
    def prompt(
        view: Ok[EditApplied] | Faulted[EditFault], caps: PromptCaps
    ) -> list[Part]:
        """Project a settled edit and the HLX grammar within the model budget."""
        if caps.maximum_parts == 0 or caps.maximum_text_bytes == 0:
            return []
        if isinstance(view, Ok):
            payload = view.payload
            status = (
                f"Committed {payload.before} -> {payload.after}; "
                f"{len(payload.changed)} range(s) changed"
                f"{' after rebase' if payload.rebased else ''}."
            )
        else:
            fault = view.fault
            status = f"Edit rejected ({fault.kind}): {fault.detail}"
            if fault.ranges:
                status += " Conflicts: " + ", ".join(fault.ranges)
        return [Part.text(_bounded(f"{status}\n\n{_DIALECT_DOCS}", caps.maximum_text_bytes))]

    @staticmethod
    def lift(from_rev: Rev, call: RecordedCall) -> LiftedCall | None:
        """Lift an edit@hl.1 call deterministically while preserving its verdict bytes."""
        if from_rev != Rev("hl", 1):
            return None
        try:
            old_args = json.loads(call.raw_args)
        except (UnicodeDecodeError, json.JSONDecodeError, TypeError):
            return None
        if not isinstance(old_args, dict) or not isinstance(old_args.get("input"), str):
            return None
        raw_args = json.dumps(
            {"patch": old_args["input"]}, ensure_ascii=False, separators=(",", ":")
        ).encode("utf-8")
        return LiftedCall(raw_args=raw_args, verdict=call.verdict)


@omp.device("edit", family="hlx", rev=1, place="env")
async def edit_hlx(args: EditArgs, ctx: Context) -> EditApplied | EditFault:
    """Apply one HLX patch through the Environment document authority."""
    del ctx
    path = _single_document_path(args.patch)
    if path is None:
        return EditFault(
            kind="invalid_patch",
            detail="the patch must contain sections for exactly one non-empty path",
        )

    omp.env.require(omp.env.Capability.DOC_READ, omp.env.Capability.DOC_WRITE)
    async with await omp.env.docs.open(omp.EnvPath(path)) as doc:
        try:
            result = await doc.hashline(
                args.patch,
                on_stale=OnStale.REBASE,
                format=Format.BEST_EFFORT,
            )
        except omp.env.Conflict as conflict:
            return EditFault(
                kind="conflict",
                detail=conflict.message,
                expected=str(conflict.expected) if conflict.expected is not None else None,
                current=str(conflict.current) if conflict.current is not None else None,
                ranges=[str(item) for item in conflict.ranges],
            )
        except omp.env.EnvError as error:
            return EditFault(kind=type(error).__name__.lower(), detail=error.message)

    return EditApplied(
        before=str(result.previous),
        after=str(result.revision),
        rebased=bool(result.rebased),
        formatted=bool(result.formatted),
        changed=[str(item) for item in result.changed_ranges],
    )


edit_hlx.prompt = _DialectProjection.prompt
edit_hlx.lift = _DialectProjection.lift
