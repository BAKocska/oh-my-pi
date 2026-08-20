"""Frozen historical session index, usage, and management surface."""

from __future__ import annotations

from collections.abc import AsyncIterator, Mapping, Sequence
from dataclasses import dataclass, field
from enum import StrEnum
from typing import Any

from _omp import Duration, EnvPath, OmpError

from ._errors import NotWiredError


class SessionError(OmpError):
    """Base error for historical session operations."""


class SessionAccessDenied(SessionError):
    """The caller may not read the requested historical session."""

    def __init__(self, session_id: str) -> None:
        self.session_id = session_id
        super().__init__(f"historical session {session_id!r} is not readable")


class SessionNotFound(OmpError):
    """The requested session does not exist or is not visible to the caller."""


class SessionStatus(StrEnum):
    """Disposition derived from the latest durable turn records."""

    COMPLETE = "complete"
    INTERRUPTED = "interrupted"
    ABORTED = "aborted"
    ERROR = "error"
    PENDING = "pending"
    UNKNOWN = "unknown"


class SessionKind(StrEnum):
    """Runtime role represented by a session index row."""

    INTERACTIVE = "interactive"
    SUBAGENT = "subagent"
    ADVISOR = "advisor"


class TitleSource(StrEnum):
    """Authority that assigned a session title."""

    USER = "user"
    MODEL = "model"
    SYSTEM = "system"


class GroupBy(StrEnum):
    """Available dimensions for indexed usage aggregation."""

    MODEL = "model"
    PROVIDER = "provider"
    PROJECT = "project"
    SESSION = "session"
    KIND = "kind"


class Bucket(StrEnum):
    """Time bucket applied to usage series output."""

    NONE = "none"
    HOUR = "hour"
    DAY = "day"
    WEEK = "week"
    MONTH = "month"


class UsageAccuracy(StrEnum):
    """Provenance of token counts in a usage aggregate."""

    EXACT = "exact"
    ESTIMATED = "estimated"
    MIXED = "mixed"


@dataclass(frozen=True, slots=True)
class Usage:
    """Unabridged token accounting stored in the sessions index."""

    input: int = 0
    output: int = 0
    cache_read: int = 0
    cache_write: int = 0
    reasoning: int = 0
    premium_requests: int = 0
    context: int | None = None
    total: int = 0
    accuracy: UsageAccuracy = UsageAccuracy.EXACT
    detail: Mapping[str, int | str] = field(default_factory=dict)


@dataclass(frozen=True, slots=True)
class Cost:
    """Nano-USD cost aggregate with a display-only USD projection."""

    nanos_usd: int = 0
    estimated: bool = False

    input_nanos_usd: int | None = None
    output_nanos_usd: int | None = None
    
    @property
    def usd(self) -> float:
        """Return the display value in USD."""

        return self.nanos_usd / 1_000_000_000


@dataclass(frozen=True, slots=True)
class SessionInfo:
    """Frozen row from the write-time sessions index."""

    id: str
    title: str | None
    title_source: TitleSource
    cwd: EnvPath
    project: str
    created_ms: int
    updated_ms: int
    status: SessionStatus
    kind: SessionKind
    parent: str | None
    entries: int
    turns: int
    usage: Usage
    cost: Cost
    models: Sequence[str]
    remote: bool


@dataclass(frozen=True, slots=True)
class SessionLink:
    """One durable parent relation in a session lineage chain."""

    id: str
    parent: str | None
    at: int | None = None


@dataclass(frozen=True, slots=True)
class SessionFilter:
    """Indexed filters for session listing and usage queries."""

    project: str | None = None
    since_ms: int | None = None
    until_ms: int | None = None
    status: Sequence[SessionStatus] | None = None
    kind: Sequence[SessionKind] | None = (SessionKind.INTERACTIVE,)
    contains_kind: str | None = None
    limit: int = 200


@dataclass(frozen=True, slots=True)
class UsageQuery:
    """Grouping and time bounds for a durable usage aggregation."""

    since_ms: int | None = None
    until_ms: int | None = None
    group_by: Sequence[GroupBy] = (GroupBy.MODEL,)
    bucket: Bucket = Bucket.NONE
    filter: SessionFilter | None = None
    include_subagents: bool = True


@dataclass(frozen=True, slots=True)
class UsageBucket:
    """One total, grouping row, or time-series bucket."""

    key: Mapping[str, str]
    start_ms: int | None
    usage: Usage
    cost: Cost
    requests: int
    errors: int
    duration: Duration


@dataclass(frozen=True, slots=True)
class UsageReport:
    """Complete result of one indexed usage query."""

    total: UsageBucket
    groups: Sequence[UsageBucket]
    series: Sequence[UsageBucket]
    sessions: int
    truncated: bool


def current() -> SessionInfo:
    """Read the current session's index projection."""

    raise NotWiredError("omp.sessions.current")


async def list(filter: SessionFilter | None = None) -> Sequence[SessionInfo]:
    """List visible sessions by newest indexed activity."""

    del filter
    raise NotWiredError("omp.sessions.list")


async def _request(operation: str, /, **arguments: object) -> Any:
    """Dispatch one session operation through the installed CONTROL bridge."""

    from . import _control_backend, _control_request

    if _control_backend.get() is None:
        raise NotWiredError(operation)
    return await _control_request(operation, **arguments)


async def get(session_id: str) -> SessionInfo:
    """Return one visible session's indexed metadata."""

    return await _request("omp.sessions.get", session_id=session_id)


async def lineage(session_id: str) -> Sequence[SessionLink]:
    """Return the durable lineage reaching a session, oldest first."""

    return await _request("omp.sessions.lineage", session_id=session_id)


async def resume(session_id: str) -> SessionInfo:
    """Resume an interactive session and journal the host transition receipt."""

    return await _request("omp.sessions.resume", session_id=session_id)


async def rename(session_id: str, title: str) -> SessionInfo:
    """Assign a user title and journal the durable rename receipt."""

    return await _request("omp.sessions.rename", session_id=session_id, title=title)


async def delete(session_id: str) -> None:
    """Delete only through a Core-approved policy ticket.

    This operation never bypasses approval.  Without the approval grant the
    Core rejects the request with :class:`omp.PermissionDenied`.
    """

    await _request("omp.sessions.delete", session_id=session_id)


async def usage(query: UsageQuery) -> UsageReport:
    """Aggregate token and cost usage from the write-time index."""

    del query
    raise NotWiredError("omp.sessions.usage")


async def journal(
    session_id: str,
    *,
    kinds: Sequence[str] | None = None,
    since: object | None = None,
    until: object | None = None,
    live: bool = True,
) -> AsyncIterator[Any]:
    """Stream decoded historical entries through the future storage host arm."""

    del session_id, kinds, since, until, live
    raise NotWiredError("omp.sessions.journal")
    yield


__all__ = (
    "Bucket", "Cost", "GroupBy", "SessionAccessDenied", "SessionError",
    "SessionFilter", "SessionInfo", "SessionKind", "SessionLink", "SessionNotFound",
    "SessionStatus", "TitleSource", "Usage",
    "UsageAccuracy", "UsageBucket", "UsageQuery", "UsageReport", "current", "delete",
    "get", "journal", "lineage", "list", "rename", "resume", "usage",
)
