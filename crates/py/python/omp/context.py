"""Frozen context-window views, patches, and compaction control."""

from __future__ import annotations

from collections.abc import AsyncIterator, Iterable, Iterator, Mapping, Sequence
from contextlib import asynccontextmanager
from contextvars import ContextVar
from dataclasses import dataclass, field
from enum import StrEnum
from itertools import groupby
from typing import TypeAlias
import base64
import sys
import uuid

from _omp import ArtifactUrl, Duration, OmpError

from . import Fault
from ._verdicts import BlobPart, JsonPart, Part, Payload, TextPart


class MessageKind(StrEnum):
    """Classify an item in the live context projection."""

    SYSTEM = "system"
    USER = "user"
    ASSISTANT = "assistant"
    TOOL_CALL = "tool_call"
    TOOL_RESULT = "tool_result"
    COMPACTION = "compaction"
    BRANCH_SUMMARY = "branch_summary"
    NOTICE = "notice"
    CUSTOM = "custom"


@dataclass(frozen=True, slots=True)
class ToolRef:
    """Identify the tool revision associated with a context item."""

    name: str
    family: str
    rev: int

    def __str__(self) -> str:
        """Render the durable tool identity."""

        return f"{self.name}@{self.family}.{self.rev}"

def _payload(response: object, schema: str) -> object:
    """Unwrap one revisioned CONTROL response while accepting direct test backends."""

    if isinstance(response, Mapping) and "schema" in response:
        if response["schema"] != schema:
            raise TypeError(
                f"expected {schema} response, got {response['schema']!r}"
            )
        if "result" in response:
            return response["result"]
    return response


def _mapping(value: object, label: str) -> Mapping[str, object]:
    if not isinstance(value, Mapping):
        raise TypeError(f"{label} must be a mapping")
    return value


def _integer(value: object, label: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool):
        raise TypeError(f"{label} must be an integer")
    return value

def _boolean(value: object, label: str) -> bool:
    if not isinstance(value, bool):
        raise TypeError(f"{label} must be a boolean")
    return value


def _tool_ref(value: object) -> ToolRef | None:
    if value is None or isinstance(value, ToolRef):
        return value
    row = _mapping(value, "context tool reference")
    return ToolRef(
        name=str(row["name"]),
        family=str(row["family"]),
        rev=_integer(row["rev"], "tool revision"),
    )


def _artifact_url(value: object) -> ArtifactUrl:
    if isinstance(value, ArtifactUrl):
        return value
    if not isinstance(value, str):
        raise TypeError("context artifact URL must be a string")
    return ArtifactUrl(value)


def _usage(value: object) -> ContextUsage:
    if isinstance(value, ContextUsage):
        return value
    row = _mapping(value, "context usage")
    return ContextUsage(
        total_tokens=_integer(row["total_tokens"], "total_tokens"),
        context_window=_integer(row["context_window"], "context_window"),
        reserve_tokens=_integer(row["reserve_tokens"], "reserve_tokens"),
        usable_tokens=_integer(row["usable_tokens"], "usable_tokens"),
        fraction=float(row["fraction"]),
        prompt_head_tokens=_integer(
            row["prompt_head_tokens"], "prompt_head_tokens"
        ),
        device_catalog_tokens=_integer(
            row["device_catalog_tokens"], "device_catalog_tokens"
        ),
        message_tokens=_integer(row["message_tokens"], "message_tokens"),
        catalog_notice_tokens=_integer(
            row["catalog_notice_tokens"], "catalog_notice_tokens"
        ),
        media_tokens=_integer(row["media_tokens"], "media_tokens"),
        compaction_epoch=_integer(row["compaction_epoch"], "compaction_epoch"),
        threshold_fraction=float(row["threshold_fraction"]),
        in_flight=_boolean(row["in_flight"], "in_flight"),
    )


def _message_ref(value: object) -> MessageRef:
    if isinstance(value, MessageRef):
        return value
    row = _mapping(value, "context message")
    artifacts = row["artifacts"]
    if not isinstance(artifacts, Sequence) or isinstance(
        artifacts, (str, bytes, bytearray)
    ):
        raise TypeError("context message artifacts must be a sequence")
    return MessageRef(
        id=str(row["id"]),
        event=_integer(row["event"], "message event"),
        seq=_integer(row["seq"], "message sequence"),
        kind=MessageKind(row["kind"]),
        role=str(row["role"]),
        turn_id=None if row["turn_id"] is None else str(row["turn_id"]),
        created_at_ms=_integer(row["created_at_ms"], "message created_at_ms"),
        tokens=_integer(row["tokens"], "message tokens"),
        byte_len=_integer(row["byte_len"], "message byte_len"),
        part_count=_integer(row["part_count"], "message part_count"),
        media_count=_integer(row["media_count"], "message media_count"),
        tool=_tool_ref(row["tool"]),
        is_error=_boolean(row["is_error"], "message is_error"),
        useless=_boolean(row["useless"], "message useless"),
        pinned=_boolean(row["pinned"], "message pinned"),
        elided=_boolean(row["elided"], "message elided"),
        superseded_by=(
            None
            if row["superseded_by"] is None
            else str(row["superseded_by"])
        ),
        artifacts=tuple(_artifact_url(item) for item in artifacts),
        preview=str(row["preview"]),
    )


def _part(value: object) -> TextPart | JsonPart | BlobPart:
    if isinstance(value, (TextPart, JsonPart, BlobPart)):
        return value
    row = _mapping(value, "context part")
    kind = row.get("kind")
    if kind == "text":
        return Part.text(str(row["text"]))
    if kind == "json":
        return Part.json(row["value"])
    if kind == "blob":
        from _omp import BlobRef

        raw_hash = row["hash"]
        if not isinstance(raw_hash, str):
            raise TypeError("blob part hash must be hexadecimal text")
        blob = BlobRef(bytes.fromhex(raw_hash), _integer(row["size"], "blob size"))
        alt = row.get("alt")
        if alt is not None and not isinstance(alt, str):
            raise TypeError("blob part alt must be a string or None")
        return Part.blob(blob, alt)
    raise TypeError(f"unknown context part kind {kind!r}")


def _verdict(value: object) -> Payload | Fault:
    if isinstance(value, (Payload, Fault)):
        return value
    row = _mapping(value, "context verdict")
    type_name = row.get("type")
    body = row.get("value")
    if not isinstance(type_name, str) or ":" not in type_name:
        raise TypeError("context verdict lacks a declared Python type")
    module_name, qualname = type_name.split(":", 1)
    target: object = sys.modules.get(module_name)
    if target is None:
        raise TypeError(f"context verdict module {module_name!r} is not loaded")
    for component in qualname.split("."):
        if component == "<locals>" or not hasattr(target, component):
            raise TypeError(f"unknown context verdict type {type_name!r}")
        target = getattr(target, component)
    if not isinstance(target, type) or not issubclass(target, (Payload, Fault)):
        raise TypeError(f"context verdict type {type_name!r} is not declared")
    if not isinstance(body, Mapping):
        raise TypeError("context verdict value must be a mapping")
    verdict = target(**body)
    if not isinstance(verdict, (Payload, Fault)):
        raise TypeError(f"context verdict type {type_name!r} decoded incorrectly")
    return verdict


@dataclass(frozen=True, slots=True)
class MessageRef:
    """Provide an immutable body-free handle to one live thread item."""

    id: str
    event: int
    seq: int
    kind: MessageKind
    role: str
    turn_id: str | None
    created_at_ms: int
    tokens: int
    byte_len: int
    part_count: int
    media_count: int
    tool: ToolRef | None
    is_error: bool
    useless: bool
    pinned: bool
    elided: bool
    superseded_by: str | None
    artifacts: tuple[ArtifactUrl, ...]
    preview: str

    async def parts(self) -> list[Part]:
        """Pull this item's model-facing parts from the host."""

        from . import _control_request

        response = await _control_request(
            "omp.context.message.parts", id=self.id, event=self.event, seq=self.seq
        )
        result = _payload(response, "omp.context.message.parts.v1")
        if not isinstance(result, Sequence) or isinstance(
            result, (str, bytes, bytearray)
        ):
            raise TypeError("omp.context.message.parts returned a non-sequence")
        return [_part(item) for item in result]

    async def verdict(self) -> Payload | Fault:
        """Pull this tool result's durable structured verdict from the host."""

        from . import _control_request

        response = await _control_request(
            "omp.context.message.verdict", id=self.id, event=self.event, seq=self.seq
        )
        return _verdict(_payload(response, "omp.context.message.verdict.v1"))

    async def raw_args(self) -> bytes | None:
        """Pull this tool call's uncorrected argument emission from the host."""

        from . import _control_request

        response = await _control_request(
            "omp.context.message.raw_args", id=self.id, event=self.event, seq=self.seq
        )
        result = _payload(response, "omp.context.message.raw_args.v1")
        if result is None or isinstance(result, bytes):
            return result
        row = _mapping(result, "raw argument result")
        encoded = row.get("base64")
        if not isinstance(encoded, str):
            raise TypeError("raw argument result must contain base64 text")
        try:
            return base64.b64decode(encoded, validate=True)
        except ValueError as error:
            raise TypeError("raw argument result contains invalid base64") from error


@dataclass(frozen=True, slots=True)
class ContextUsage:
    """Summarize token usage and compaction pressure for the live context."""

    total_tokens: int
    context_window: int
    reserve_tokens: int
    usable_tokens: int
    fraction: float
    prompt_head_tokens: int
    device_catalog_tokens: int
    message_tokens: int
    catalog_notice_tokens: int
    media_tokens: int
    compaction_epoch: int
    threshold_fraction: float
    in_flight: bool


@dataclass(frozen=True, slots=True)
class ContextView:
    """Expose an immutable projection of the current model context."""

    session_id: str
    turn_id: str
    model: str
    provider: str
    epoch: int
    messages: tuple[MessageRef, ...]
    usage: ContextUsage
    prompt_hash: str
    reset_event: int | None

    def since(self, turn_id: str) -> Iterator[MessageRef]:
        """Iterate items at or after the named turn."""

        found = False
        for message in self.messages:
            if message.turn_id == turn_id:
                found = True
            if found:
                yield message

    def by_turn(self) -> Iterator[tuple[str | None, tuple[MessageRef, ...]]]:
        """Group projected items into consecutive turns in projection order."""

        for turn_id, messages in groupby(self.messages, key=lambda message: message.turn_id):
            yield turn_id, tuple(messages)

    def tokens_of(self, ids: Iterable[str]) -> int:
        """Sum token counts for the named item identifiers."""

        selected = frozenset(ids)
        return sum(message.tokens for message in self.messages if message.id in selected)


@dataclass(frozen=True, slots=True)
class Prune:
    """Remove named items from one turn's working context copy."""

    ids: tuple[str, ...]
    reason: str = ""
    keep_placeholder: bool = True


@dataclass(frozen=True, slots=True)
class DropParts:
    """Drop parts only from the model-facing projection; retain typed verdict and journal."""

    ids: tuple[str, ...]
    reason: str = ""


@dataclass(frozen=True, slots=True)
class Replace:
    """Replace named context items with one synthetic item."""

    ids: tuple[str, ...]
    parts: tuple[Part, ...]
    role: str = "user"
    label: str = ""
    inherit_position: str = "first"


@dataclass(frozen=True, slots=True)
class Anchor:
    """Locate an inserted synthetic item relative to the live context."""

    relation: str
    id: str | None = None

    def __post_init__(self) -> None:
        """Reject inconsistent anchor relations and identifiers."""

        if self.relation in {"before", "after"}:
            if self.id is None:
                raise ValueError(f"{self.relation} anchor requires an id")
        elif self.relation in {"head", "tail"}:
            if self.id is not None:
                raise ValueError(f"{self.relation} anchor does not accept an id")
        else:
            raise ValueError(f"unknown anchor relation {self.relation!r}")

    @staticmethod
    def before(id: str) -> Anchor:
        """Anchor immediately before a named item."""

        return Anchor("before", id)

    @staticmethod
    def after(id: str) -> Anchor:
        """Anchor immediately after a named item."""

        return Anchor("after", id)

    @staticmethod
    def head() -> Anchor:
        """Anchor after the prompt head and before conversation items."""

        return Anchor("head")

    @staticmethod
    def tail() -> Anchor:
        """Anchor immediately before the pending user turn."""

        return Anchor("tail")


@dataclass(frozen=True, slots=True)
class Insert:
    """Insert a synthetic item into one turn's working context copy."""

    parts: tuple[Part, ...]
    anchor: Anchor
    role: str = "user"
    ephemeral: bool = True
    dedupe_key: str | None = None


@dataclass(frozen=True, slots=True)
class Reorder:
    """Move named context items before another item while preserving order."""

    ids: tuple[str, ...]
    before: str


@dataclass(slots=True)
class ContextPatch:
    """Collect context projection operations contributed by one handler."""

    prune: list[Prune] = field(default_factory=list)
    drop_parts: list[DropParts] = field(default_factory=list)
    replace: list[Replace] = field(default_factory=list)
    insert: list[Insert] = field(default_factory=list)
    reorder: list[Reorder] = field(default_factory=list)
    note: str = ""

    def is_empty(self) -> bool:
        """Return whether this patch contains no operations."""

        return not (
            self.prune
            or self.drop_parts
            or self.replace
            or self.insert
            or self.reorder
        )

    def merge(self, other: ContextPatch) -> ContextPatch:
        """Return a patch concatenating this patch's operations with another's."""

        note = "; ".join(note for note in (self.note, other.note) if note)
        return ContextPatch(
            prune=[*self.prune, *other.prune],
            drop_parts=[*self.drop_parts, *other.drop_parts],
            replace=[*self.replace, *other.replace],
            insert=[*self.insert, *other.insert],
            reorder=[*self.reorder, *other.reorder],
            note=note,
        )


class CompactionTier(StrEnum):
    """Select one rung of the context compaction ladder."""

    PRUNE = "prune"
    DROP_MEDIA = "drop_media"
    ELIDE = "elide"
    LOCAL = "local"
    REMOTE = "remote"
    HANDOFF = "handoff"


@dataclass(frozen=True, slots=True)
class CompactionEvent:
    """Describe one pending rung of the context compaction ladder."""

    preparation_id: str
    tier: CompactionTier
    reason: str
    epoch: int
    tokens_before: int
    target_tokens: int
    suggested_first_kept: str
    to_summarize: tuple[MessageRef, ...]
    to_retain: tuple[MessageRef, ...]
    split_turn: bool
    previous_summary: str | None
    previous_preserve: dict | None
    custom_instructions: str | None
    deadline: Duration


@dataclass(frozen=True, slots=True)
class CancelCompaction:
    """Skip one compaction tier and optionally suppress later ladders."""

    reason: str
    suppress_for_turns: int = 0


@dataclass(frozen=True, slots=True)
class CustomSummary:
    """Replace a compaction tier with an extension-authored summary."""

    summary: str
    first_kept_id: str
    short: str | None = None
    warning: str | None = None
    details: dict | None = None
    preserve: dict | None = None


@dataclass(frozen=True, slots=True)
class DelegateCompaction:
    """Run the default compaction tier with extension-supplied adjustments."""

    extra_instructions: str = ""
    focus_ids: tuple[str, ...] = ()
    role: str | None = None
    keep_recent_tokens: int | None = None


CompactionVerdict: TypeAlias = CancelCompaction | CustomSummary | DelegateCompaction
"""Typed return accepted from a compaction domain hook."""


@dataclass(frozen=True, slots=True)
class CompactionOutcome:
    """Report the durable result of a completed compaction request."""

    preparation_id: str
    tiers_run: tuple[CompactionTier, ...]
    from_extension: str | None
    tokens_before: int
    tokens_after: int
    first_kept_id: str
    epoch: int
    summary_bytes: int
    warning: str | None


@dataclass(frozen=True, slots=True)
class ContextResetEvent:
    """Describe a reset boundary that replaced the live context chain."""

    reset_event: int
    epoch: int
    kind: str
    tokens_discarded: int
    last_turn_id: str | None


class CompactionBusy(OmpError):
    """Compaction was requested while another compaction was running."""


class CompactionRefused(OmpError):
    """A compaction verdict attempted to cancel an unavoidable rescue handoff."""


class PatchRejected(OmpError):
    """A context patch or compaction verdict violated a structural rule."""


class ContextGone(OmpError):
    """A message handle named an item no longer in the live chain."""


class NoVerdict(OmpError):
    """A message has no durable structured verdict available."""


class PinBudgetExceeded(OmpError):
    """A pin request would exceed the configured context-window budget."""


class StaleEpoch(OmpError):
    """A strict context lane attempted a write after its epoch changed."""


async def view() -> ContextView:
    """Fetch the current context projection from the host."""

    from . import _control_request

    response = _payload(
        await _control_request("omp.context.view"), "omp.context.view.v1"
    )
    if isinstance(response, ContextView):
        return response
    row = _mapping(response, "context view")
    messages = row["messages"]
    if not isinstance(messages, Sequence) or isinstance(
        messages, (str, bytes, bytearray)
    ):
        raise TypeError("context view messages must be a sequence")
    reset_event = row["reset_event"]
    return ContextView(
        session_id=str(row["session_id"]),
        turn_id=str(row["turn_id"]),
        model=str(row["model"]),
        provider=str(row["provider"]),
        epoch=_integer(row["epoch"], "context epoch"),
        messages=tuple(_message_ref(item) for item in messages),
        usage=_usage(row["usage"]),
        prompt_hash=str(row["prompt_hash"]),
        reset_event=(
            None
            if reset_event is None
            else _integer(reset_event, "context reset event")
        ),
    )


async def usage() -> ContextUsage:
    """Fetch current context usage without building a projection."""

    from . import _control_request

    response = await _control_request("omp.context.usage")
    return _usage(_payload(response, "omp.context.usage.v1"))


async def pin(ids: Iterable[str], *, reason: str) -> int:
    """Durably protect context items from patches and compaction."""

    from . import _control_request

    if not isinstance(reason, str):
        raise TypeError("reason must be a string")
    response = await _control_request(
        "omp.context.pin",
        ids=_wire_ids(ids),
        reason=reason,
        idempotency_key=uuid.uuid4().hex,
    )
    return _integer(
        _payload(response, "omp.context.pin.v1"), "context pin count"
    )


async def unpin(ids: Iterable[str]) -> int:
    """Release context pins owned by the calling extension."""

    from . import _control_request

    response = await _control_request(
        "omp.context.unpin",
        ids=_wire_ids(ids),
        idempotency_key=uuid.uuid4().hex,
    )
    return _integer(
        _payload(response, "omp.context.unpin.v1"), "context unpin count"
    )


async def compact(
    *, tier: CompactionTier | None = None, focus: str = ""
) -> CompactionOutcome:
    """Request out-of-band context compaction from the host."""

    from . import _control_request

    if tier is not None and not isinstance(tier, CompactionTier):
        raise TypeError("tier must be CompactionTier or None")
    if not isinstance(focus, str):
        raise TypeError("focus must be a string")
    response = _payload(
        await _control_request(
            "omp.context.compact",
            tier=None if tier is None else tier.value,
            focus=focus,
            idempotency_key=uuid.uuid4().hex,
        ),
        "omp.context.compact.v1",
    )
    if isinstance(response, CompactionOutcome):
        return response
    row = _mapping(response, "compaction outcome")
    tiers = row["tiers_run"]
    if not isinstance(tiers, Sequence) or isinstance(
        tiers, (str, bytes, bytearray)
    ):
        raise TypeError("compaction tiers_run must be a sequence")
    warning = row["warning"]
    extension = row["from_extension"]
    return CompactionOutcome(
        preparation_id=str(row["preparation_id"]),
        tiers_run=tuple(CompactionTier(value) for value in tiers),
        from_extension=None if extension is None else str(extension),
        tokens_before=_integer(row["tokens_before"], "tokens_before"),
        tokens_after=_integer(row["tokens_after"], "tokens_after"),
        first_kept_id=str(row["first_kept_id"]),
        epoch=_integer(row["epoch"], "compaction epoch"),
        summary_bytes=_integer(row["summary_bytes"], "summary_bytes"),
        warning=None if warning is None else str(warning),
    )


async def epoch() -> int:
    """Fetch the current durable context compaction epoch."""

    from . import _control_request

    response = await _control_request("omp.context.epoch")
    return _integer(_payload(response, "omp.context.epoch.v1"), "context epoch")


@dataclass(frozen=True, slots=True)
class _LaneState:
    strict_epoch: bool
    entered_epoch: int | None


_lane_active: ContextVar[_LaneState | None] = ContextVar(
    "omp_context_lane", default=None
)


def _wire_ids(ids: Iterable[str]) -> list[str]:
    if isinstance(ids, (str, bytes, bytearray)):
        raise TypeError("context ids must be an iterable of strings")
    result = list(ids)
    if any(not isinstance(item, str) or not item for item in result):
        raise TypeError("context ids must be non-empty strings")
    return result


def _journal_epoch_fence() -> int | None:
    state = _lane_active.get()
    if state is None or not state.strict_epoch:
        return None
    return state.entered_epoch


@asynccontextmanager
async def lane(*, strict_epoch: bool = False) -> AsyncIterator[None]:
    """Mark an asynchronous block as deprioritized auxiliary context work."""

    if not isinstance(strict_epoch, bool):
        raise TypeError("strict_epoch must be bool")
    entered_epoch = await epoch() if strict_epoch else None
    token = _lane_active.set(_LaneState(strict_epoch, entered_epoch))
    try:
        yield
    finally:
        _lane_active.reset(token)


__all__ = (
    "Anchor",
    "CancelCompaction",
    "CompactionBusy",
    "CompactionEvent",
    "CompactionOutcome",
    "CompactionRefused",
    "CompactionTier",
    "CompactionVerdict",
    "ContextGone",
    "ContextPatch",
    "ContextResetEvent",
    "ContextUsage",
    "ContextView",
    "CustomSummary",
    "DelegateCompaction",
    "DropParts",
    "Insert",
    "MessageKind",
    "MessageRef",
    "NoVerdict",
    "PatchRejected",
    "PinBudgetExceeded",
    "Prune",
    "Reorder",
    "Replace",
    "StaleEpoch",
    "ToolRef",
    "compact",
    "epoch",
    "lane",
    "pin",
    "unpin",
    "usage",
    "view",
)
