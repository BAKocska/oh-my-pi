"""Typed session-journal declarations whose host backing arrives after FREEZE.

Importing this module performs no I/O. The functions intentionally fail at use
until the host installs the CONTROL journal implementation.
"""

from __future__ import annotations

from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass
import json
from typing import Any, Generic, TypeVar

from _omp import OmpError

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

    @classmethod
    def parse(cls, value: str) -> EntryId:
        """Parse the canonical ``<session_id>:<index>`` representation."""

        if not isinstance(value, str):
            raise TypeError("entry id must be a string")
        session, separator, raw_index = value.rpartition(":")
        if (
            not separator
            or not session
            or not raw_index.isascii()
            or not raw_index.isdecimal()
            or (len(raw_index) > 1 and raw_index.startswith("0"))
        ):
            raise ValueError(f"invalid entry id: {value!r}")
        return cls(session=session, index=int(raw_index))

    def __str__(self) -> str:
        """Render this id as ``<session_id>:<index>``."""

        return f"{self.session}:{self.index}"


class JournalError(OmpError):
    """Base error for journal operations and partial multi-entry appends."""

    def __init__(
        self,
        message: str,
        *,
        appended: Iterable[EntryId] = (),
    ) -> None:
        super().__init__(message)
        self.appended: list[EntryId] = list(appended)


class UnknownEntryKind(JournalError):
    """An append payload is not an instance of a declared entry kind."""

    def __init__(self, kind: object) -> None:
        self.kind = kind
        super().__init__(f"unknown journal entry kind: {kind!r}")


class EntryKindConflict(JournalError):
    """An entry-kind name is already owned by another declaration."""

    def __init__(self, name: str, owner: str | None = None) -> None:
        self.name = name
        self.owner = owner
        detail = f" by {owner!r}" if owner is not None else ""
        super().__init__(f"journal entry kind {name!r} is already owned{detail}")


class EntryTooLarge(JournalError):
    """A journal entry exceeds its applicable encoded-size ceiling."""

    def __init__(self, actual: int, limit: int) -> None:
        self.actual = actual
        self.limit = limit
        super().__init__(f"journal entry is {actual} bytes; limit is {limit}")


class EntryAccessDenied(JournalError):
    """The caller may not read the requested entry-kind namespace."""

    def __init__(self, kind: str) -> None:
        self.kind = kind
        super().__init__(f"journal entry kind {kind!r} is not readable")


class JournalIndeterminate(JournalError):
    """A journal mutation's durability could not be proven."""

    def __init__(
        self, operation: str = "journal mutation", *, appended: Iterable[EntryId] = ()
    ) -> None:
        self.operation = operation
        super().__init__(
            f"{operation} has an indeterminate durability outcome", appended=appended
        )


class EntryUndecodable(JournalError):
    """Canonical entry bytes could not be decoded without repair."""

    def __init__(self, raw: bytes, reason: str) -> None:
        self.raw = raw
        self.reason = reason
        super().__init__(f"journal entry bytes are not canonical: {reason}")


@dataclass(frozen=True, slots=True, order=True)
class StateEntryId:
    """Opaque, totally ordered physical index within one scoped state log."""

    scope: str
    index: int

    def __str__(self) -> str:
        """Render this id as ``<scope_instance>:<index>``."""

        return f"{self.scope}:{self.index}"


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


@dataclass(frozen=True, slots=True)
class StateEntry(Generic[_T]):
    """Immutable decoded view of one durable scoped-state record."""

    id: StateEntryId
    kind: str
    rev: str
    ts: int
    principal: object
    provenance: object
    value: _T | None
    raw: bytes
    artifact: ArtifactRef | None = None


def _unavailable() -> NotWiredError:
    return NotWiredError("omp.journal CONTROL backing is not wired")

def decode(raw: bytes) -> Any:
    """Decode only the exact canonical JSON encoding written by the host."""

    if not isinstance(raw, bytes):
        raise TypeError("journal entry data must be bytes")

    def reject_constant(value: str) -> None:
        raise ValueError(f"non-finite number {value!r}")

    try:
        value = json.loads(raw.decode("utf-8"), parse_constant=reject_constant)
        canonical = json.dumps(
            value,
            ensure_ascii=False,
            allow_nan=False,
            separators=(",", ":"),
            sort_keys=True,
        ).encode("utf-8")
    except (UnicodeError, ValueError, TypeError, OverflowError) as error:
        raise EntryUndecodable(raw, str(error)) from error
    if canonical != raw:
        raise EntryUndecodable(raw, "encoding differs from canonical JSON")
    return value


def label(target: EntryId, label: str | None) -> EntryId:
    """Append a label event for an addressable journal entry."""

    if not isinstance(target, EntryId):
        raise TypeError("target must be an EntryId")
    if label is not None:
        if not isinstance(label, str):
            raise TypeError("label must be a string or None")
        encoded_length = len(label.encode("utf-8"))
        if encoded_length > MAX_LABEL_BYTES:
            raise JournalError(
                f"journal label is {encoded_length} bytes; limit is {MAX_LABEL_BYTES}"
            )
    raise _unavailable()


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
    "EntryAccessDenied",
    "EntryId",
    "EntryKindConflict",
    "EntryTooLarge",
    "EntryUndecodable",
    "JournalError",
    "JournalIndeterminate",
    "JournalEntry",
    "MAX_ATOMIC_ENTRIES",
    "MAX_ENTRY_BYTES",
    "MAX_INLINE_BYTES",
    "MAX_LABEL_BYTES",
    "StateEntry",
    "StateEntryId",
    "UnknownEntryKind",
    "append",
    "append_atomic",
    "append_many",
    "decode",
    "entries",
    "fold",
    "label",
    "latest",
)
