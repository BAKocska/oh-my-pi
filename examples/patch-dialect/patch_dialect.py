from __future__ import annotations

import base64
import dataclasses
import json

import omp
from omp import Context, Faulted, LiftedCall, Ok, Part, Payload, PromptCaps, RecordedCall, Rev
from omp.env import Edit, Format, OnStale

_REPLACE_REV = Rev.parse("rep.1")
_HASHLINE_REV = Rev.parse("hl.1")
_DIALECT_DOCS = """Patch-envelope edit dialect
Pass `patch` with one path, the pinned revision `tag`, and line-anchored `hunks`. Line numbers are one-based and inclusive; `replacement` is exact UTF-8 text. The tag must equal the document lease's current content revision.
"""


@dataclasses.dataclass(frozen=True, slots=True)
class PatchHunk:
    """One inclusive line-range replacement in a patch envelope."""

    start_line: int
    end_line: int
    replacement: str


@dataclasses.dataclass(frozen=True, slots=True)
class PatchEnvelope:
    """One tag-verified document patch."""

    path: str
    tag: str
    hunks: list[PatchHunk]


@dataclasses.dataclass(frozen=True, slots=True)
class PatchArgs:
    """Arguments for the patch-envelope dialect."""

    patch: PatchEnvelope


@dataclasses.dataclass(frozen=True, slots=True)
class AppliedEdit:
    """Dialect-neutral facts for one resolved line replacement."""

    path: str
    start_line: int
    end_line: int
    replacement: str


@dataclasses.dataclass(frozen=True, slots=True)
class PatchApplied(Payload):
    """The durable result of a tag-verified patch."""

    path: str
    before: str
    after: str
    rebased: bool
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


def _decoded_bytes(value: object) -> bytes | None:
    if isinstance(value, list) and all(
        isinstance(item, int) and not isinstance(item, bool) and 0 <= item <= 255
        for item in value
    ):
        return bytes(value)
    if isinstance(value, dict) and isinstance(value.get("$bytes"), str):
        try:
            return base64.b64decode(value["$bytes"], validate=True)
        except (ValueError, TypeError):
            return None
    return None

def _content_tag(revision: str) -> str | None:
    _sequence, separator, candidate = revision.partition(":")
    tag = candidate if separator else revision
    if len(tag) != 64 or any(character not in "0123456789abcdef" for character in tag):
        return None
    return tag


def _whole_document_hunk(before: bytes, after: bytes) -> PatchHunk | None:
    try:
        replacement = after.decode("utf-8")
        before.decode("utf-8")
    except UnicodeDecodeError:
        return None
    line_count = max(1, len(before.splitlines()))
    return PatchHunk(start_line=1, end_line=line_count, replacement=replacement)


def _lifted_values(verdict: dict[str, object]) -> tuple[PatchArgs, PatchApplied] | None:
    if verdict.get("kind") != "ok":
        return None
    value = verdict.get("value")
    if not isinstance(value, dict):
        return None
    sections = value.get("sections")
    if not isinstance(sections, list) or len(sections) != 1:
        return None
    section = sections[0]
    if not isinstance(section, dict) or section.get("op") not in ("update", "noop"):
        return None

    path = section.get("path")
    before_revision = section.get("old_revision")
    after_revision = section.get("new_revision")
    rebased = section.get("rebased")
    before = _decoded_bytes(section.get("before"))
    after = _decoded_bytes(section.get("after"))
    if (
        not isinstance(path, str)
        or not path
        or not isinstance(before_revision, str)
        or not before_revision
        or not isinstance(after_revision, str)
        or not after_revision
        or not isinstance(rebased, bool)
        or before is None
        or after is None
    ):
        return None

    before_tag = _content_tag(before_revision)
    after_tag = _content_tag(after_revision)
    hunk = _whole_document_hunk(before, after)
    if before_tag is None or after_tag is None or hunk is None:
        return None
    args = PatchArgs(patch=PatchEnvelope(path=path, tag=before_tag, hunks=[hunk]))
    applied = PatchApplied(
        path=path,
        before=before_tag,
        after=after_tag,
        rebased=rebased,
        edits=[
            AppliedEdit(
                path=path,
                start_line=hunk.start_line,
                end_line=hunk.end_line,
                replacement=hunk.replacement,
            )
        ],
    )
    return args, applied


def _byte_edits(content: bytes, hunks: list[PatchHunk]) -> list[Edit] | None:
    lines = content.splitlines(keepends=True)
    if not lines:
        lines = [b""]
    offsets = [0]
    for line in lines:
        offsets.append(offsets[-1] + len(line))

    edits: list[Edit] = []
    previous_end = 0
    for hunk in hunks:
        if (
            isinstance(hunk.start_line, bool)
            or not isinstance(hunk.start_line, int)
            or isinstance(hunk.end_line, bool)
            or not isinstance(hunk.end_line, int)
            or hunk.start_line < 1
            or hunk.end_line < hunk.start_line
            or hunk.end_line > len(lines)
            or hunk.start_line <= previous_end
        ):
            return None
        edits.append(
            Edit(
                start=offsets[hunk.start_line - 1],
                end=offsets[hunk.end_line],
                replacement=hunk.replacement.encode("utf-8"),
            )
        )
        previous_end = hunk.end_line
    return edits


def _bounded(text: str, maximum_bytes: int) -> str:
    encoded = text.encode("utf-8")
    if len(encoded) <= maximum_bytes:
        return text
    return encoded[:maximum_bytes].decode("utf-8", errors="ignore")


class _PatchProjection:
    @staticmethod
    def prompt(
        view: Ok[PatchApplied] | Faulted[PatchFault], caps: PromptCaps
    ) -> list[omp.TextPart]:
        """Project the patch-envelope grammar and settled status within budget."""
        if caps.maximum_parts == 0 or caps.maximum_text_bytes == 0:
            return []
        if isinstance(view, Ok):
            status = f"Patched {view.payload.path}: {view.payload.before} -> {view.payload.after}."
        else:
            status = f"Patch rejected ({view.fault.kind}): {view.fault.detail}"
        return [Part.text(_bounded(f"{status}\n\n{_DIALECT_DOCS}", caps.maximum_text_bytes))]

    @staticmethod
    def lift(from_rev: Rev, call: RecordedCall) -> LiftedCall | None:
        """Lift one rep.1 or hl.1 call directly into live patch.1."""
        if (
            call.identity.name != "edit"
            or call.identity.rev != from_rev
            or not _matches_source_schema(from_rev, call.raw_args)
        ):
            return None
        verdict = _json_object(call.verdict)
        if verdict is None:
            return None
        lifted = _lifted_values(verdict)
        if lifted is None:
            return None
        args, payload = lifted
        return LiftedCall.of(args, {"kind": "ok", "value": payload})


@omp.device("edit", family="patch", rev=1, place="env")
async def edit_patch(args: PatchArgs, ctx: Context) -> PatchApplied | PatchFault:
    """Commit one tag-verified line patch through a document lease."""
    del ctx
    patch = args.patch
    if not patch.path or not patch.tag or not patch.hunks:
        return PatchFault(kind="invalid_patch", detail="path, tag, and hunks must be non-empty")

    omp.env.require(omp.env.Capability.DOC_READ, omp.env.Capability.DOC_WRITE)
    async with await omp.env.docs.open(omp.EnvPath(patch.path)) as doc:
        if doc.revision is None or doc.revision.hex != patch.tag:
            current = None if doc.revision is None else doc.revision.hex
            return PatchFault(
                kind="tag_mismatch",
                detail=f"expected revision {patch.tag}, current revision is {current}",
            )
        content = await doc.read_bytes()
        edits = _byte_edits(content, patch.hunks)
        if edits is None:
            return PatchFault(
                kind="invalid_patch",
                detail="hunks must be sorted, non-overlapping, and within the pinned document",
            )
        try:
            result = await doc.edit(
                edits,
                on_stale=OnStale.REBASE,
                format=Format.BEST_EFFORT,
            )
        except omp.env.EnvError as error:
            return PatchFault(kind=type(error).__name__.lower(), detail=error.message)

    return PatchApplied(
        path=patch.path,
        before=result.previous.hex,
        after=result.revision.hex,
        rebased=result.rebased,
        edits=[
            AppliedEdit(
                path=patch.path,
                start_line=hunk.start_line,
                end_line=hunk.end_line,
                replacement=hunk.replacement,
            )
            for hunk in patch.hunks
        ],
    )


edit_patch.prompt = _PatchProjection.prompt
edit_patch.lift = _PatchProjection.lift
