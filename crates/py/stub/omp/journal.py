"""Append-only in-process journal fake for standalone extension tests."""
from __future__ import annotations

from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True, slots=True)
class Entry:
    """One journal event with a monotonic sequence assigned by the fake."""

    sequence: int
    topic: str
    value: Any


class Fake:
    """A deterministic append-only journal that never persists to disk."""

    def __init__(self) -> None:
        self._entries: list[Entry] = []

    async def append(self, topic: str, value: Any) -> Entry:
        """Append one value and return its assigned sequence entry."""
        if not isinstance(topic, str) or not topic:
            raise ValueError("journal topic must be a non-empty str")
        entry = Entry(len(self._entries), topic, value)
        self._entries.append(entry)
        return entry

    async def entries(self, topic: str | None = None) -> tuple[Entry, ...]:
        """Return entries in append order, optionally filtered by topic."""
        if topic is None:
            return tuple(self._entries)
        return tuple(entry for entry in self._entries if entry.topic == topic)


_current = Fake()


def install(fake: Fake) -> None:
    """Install the fake used by module-level journal calls."""
    global _current
    _current = fake


def reset() -> Fake:
    """Install and return a new empty journal fake."""
    global _current
    _current = Fake()
    return _current


async def append(topic: str, value: Any) -> Entry:
    """Append through the module-level fake."""
    return await _current.append(topic, value)


async def entries(topic: str | None = None) -> tuple[Entry, ...]:
    """Read module-level entries through the fake."""
    return await _current.entries(topic)


__all__ = ("Entry", "Fake", "append", "entries", "install", "reset")
