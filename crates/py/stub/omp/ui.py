"""In-process recorder for extension UI effects in standalone tests."""
from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
from typing import Any


class Level(StrEnum):
    """Severity of a recorded notification."""

    INFO = "info"
    WARN = "warn"
    ERROR = "error"


@dataclass(frozen=True, slots=True)
class Effect:
    """A data-only UI effect recorded by the fake presentation surface."""

    kind: str
    payload: dict[str, Any]


class Fake:
    """Collects UI effects synchronously and exposes them to assertions."""

    def __init__(self) -> None:
        self._effects: list[Effect] = []

    def emit(self, kind: str, **payload: Any) -> None:
        """Record one data-only effect."""
        self._effects.append(Effect(kind, dict(payload)))

    def drain(self) -> tuple[Effect, ...]:
        """Return and clear all recorded effects in emission order."""
        result = tuple(self._effects)
        self._effects.clear()
        return result


_current = Fake()


def install(fake: Fake) -> None:
    """Install the fake used by module-level UI helpers."""
    global _current
    _current = fake


def reset() -> Fake:
    """Install and return a new empty UI fake."""
    global _current
    _current = Fake()
    return _current


def notify(message: str, *, level: Level = Level.INFO, title: str | None = None) -> None:
    """Record a notification effect rather than displaying it."""
    _current.emit("notify", message=str(message), level=level, title=title)


def status(text: str) -> None:
    """Record a status-line effect."""
    _current.emit("status", text=str(text))


def effects() -> tuple[Effect, ...]:
    """Return current effects without clearing them."""
    return tuple(_current._effects)


__all__ = ("Effect", "Fake", "Level", "effects", "install", "notify", "reset", "status")
