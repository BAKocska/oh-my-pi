"""Frozen context-window views, patches, and compaction control."""

from __future__ import annotations

from collections.abc import AsyncIterator, Iterable, Iterator
from contextlib import asynccontextmanager
from contextvars import ContextVar
from dataclasses import dataclass, field
from enum import StrEnum
from itertools import groupby
from typing import TypeAlias

from _omp import ArtifactUrl, Duration, OmpError

from . import Fault
from ._errors import NotWiredError
from ._verdicts import Part, Payload


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

        del self
        raise NotWiredError("omp.context.MessageRef.parts")

    async def verdict(self) -> Payload | Fault:
        """Pull this tool result's durable structured verdict from the host."""

        del self
        raise NotWiredError("omp.context.MessageRef.verdict")

    async def raw_args(self) -> bytes | None:
        """Pull this tool call's uncorrected argument emission from the host."""

        del self
        raise NotWiredError("omp.context.MessageRef.raw_args")


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

    raise NotWiredError("omp.context.view")


async def usage() -> ContextUsage:
    """Fetch current context usage without building a projection."""

    raise NotWiredError("omp.context.usage")


async def pin(ids: Iterable[str], *, reason: str) -> int:
    """Durably protect context items from patches and compaction."""

    del ids, reason
    raise NotWiredError("omp.context.pin")


async def unpin(ids: Iterable[str]) -> int:
    """Release context pins owned by the calling extension."""

    del ids
    raise NotWiredError("omp.context.unpin")


async def compact(
    *, tier: CompactionTier | None = None, focus: str = ""
) -> CompactionOutcome:
    """Request out-of-band context compaction from the host."""

    del tier, focus
    raise NotWiredError("omp.context.compact")


async def epoch() -> int:
    """Fetch the current durable context compaction epoch."""

    raise NotWiredError("omp.context.epoch")


_lane_active: ContextVar[bool | None] = ContextVar("omp_context_lane", default=None)


@asynccontextmanager
async def lane(*, strict_epoch: bool = False) -> AsyncIterator[None]:
    """Mark an asynchronous block as deprioritized auxiliary context work."""

    token = _lane_active.set(strict_epoch)
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
