"""Typed session-journal declarations whose host backing arrives after FREEZE.

Importing this module performs no I/O. The functions intentionally fail at use
until the host installs the CONTROL journal implementation.
"""

from __future__ import annotations

from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass
from typing import Generic, TypeVar

from ._errors import NotWiredError


from ._verdicts import ArtifactRef
_T = TypeVar("_T")
_A = TypeVar("_A")

MAX_INLINE_BYTES = 65_536
"""Largest entry encoded inline before artifact spilling is required."""

MAX_ENTRY_BYTES = 16_777_216
"""Hard encoded-size ceiling for one journal entry."""

MAX_LABEL_BYTES = 256
"""Maximum UTF-8 byte length of a journal label."""

MAX_ATOMIC_ENTRIES = 1_024
"""Maximum number of entries accepted by one atomic append."""


@dataclass(frozen=True, slots=True, order=True)
class EntryId:
    """Opaque, totally ordered physical index within one session journal."""

    session: str
    index: int

    def __str__(self) -> str:
        """Render this id as ``<session_id>:<index>``."""

        return f"{self.session}:{self.index}"


@dataclass(frozen=True, slots=True)
class JournalEntry(Generic[_T]):
    """Immutable decoded view of one durable session-journal record."""

    id: EntryId
    kind: str
    rev: str
    ts: int
    principal: object
    provenance: object
    value: _T | None
    raw: bytes
    display: bool
    in_context: bool
    artifact: ArtifactRef | None = None


def _unavailable() -> NotWiredError:
    return NotWiredError("omp.journal CONTROL backing is not wired")

def append(
    entry: object,
    *,
    display: bool | None = None,
    idempotency_key: str | None = None,
) -> EntryId:
    """Append one declared entry durably once CONTROL backing is installed."""

    del entry, display, idempotency_key
    raise _unavailable()


async def append_many(
    entries: Iterable[object], *, idempotency_key: str | None = None
) -> list[EntryId]:
    """Append an ordered, non-atomic group in one CONTROL round trip."""

    del entries, idempotency_key
    raise _unavailable()


async def append_atomic(
    entries: Iterable[object], *, idempotency_key: str
) -> list[EntryId]:
    """Append an idempotent group atomically once CONTROL backing is installed."""

    del entries, idempotency_key
    raise _unavailable()


def entries(
    kind: str | type[_T] | None = None,
    *,
    rev: str | None = None,
    since: EntryId | None = None,
    limit: int | None = None,
    live: bool = True,
) -> Sequence[JournalEntry[_T]]:
    """Read ascending, optionally kind-scoped entries from the current session."""

    del kind, rev, since, limit, live
    raise _unavailable()


def latest(kind: str | type[_T]) -> JournalEntry[_T] | None:
    """Return the highest-index live entry of one declared kind."""

    rows = entries(kind, limit=1)
    return rows[0] if rows else None


def fold(
    kind: str | type[_T],
    reducer: Callable[[_A, JournalEntry[_T]], _A],
    initial: _A,
    *,
    since: EntryId | None = None,
) -> tuple[_A, EntryId | None]:
    """Fold live entries left-to-right and return the last folded watermark."""

    accumulator = initial
    watermark = None
    for entry in entries(kind, since=since):
        accumulator = reducer(accumulator, entry)
        watermark = entry.id
    return accumulator, watermark


__all__ = (
    "EntryId",
    "JournalEntry",
    "MAX_ATOMIC_ENTRIES",
    "MAX_ENTRY_BYTES",
    "MAX_INLINE_BYTES",
    "MAX_LABEL_BYTES",
    "append",
    "append_atomic",
    "append_many",
    "entries",
    "fold",
    "latest",
)
