"""Standalone, typed test double for the public ``omp`` extension API.

``omp-stub`` deliberately performs no harness discovery.  It supplies deterministic
in-process ``env``, ``ui``, and ``journal`` fakes so extension projects can use uv,
pytest, ruff, and type checkers without an embedded omp host.
"""
from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
from pathlib import PurePosixPath
from typing import Any, Callable, TypeVar

from . import diagnostics, env, journal, packages, ui
from .diagnostics import FailureCode, WarningCode


class OmpError(RuntimeError):
    """Base error raised by the standalone omp API."""


class PackageError(OmpError):
    """Package metadata is unavailable outside an installed extension host."""


class Capability(StrEnum):
    """Manifest capabilities that are relevant to the standalone API."""

    PLACE_ENV = "place_env"
    PLACE_WORKER = "place_worker"


@dataclass(frozen=True, slots=True)
class WorkspaceUri:
    """Canonical workspace identity and stable grant-key digest."""

    uri: str
    digest: str


@dataclass(frozen=True, slots=True)
class Provenance:
    """Structural provenance for an extension action."""

    publisher: str
    extension_id: str
    version: str
    artifact_digest: str
    layer: str
    tier: str
    generation: int

class Secret:
    """Redacting in-process stand-in for a host-managed secret."""

    __slots__ = ("_value",)

    def __init__(self, value: bytes) -> None:
        """Create a secret from bytes for a standalone test."""
        self._value = bytes(value)

    def __repr__(self) -> str:
        """Return a non-disclosing representation."""
        return "Secret(<redacted>)"

    __str__ = __repr__

    def __format__(self, _format_spec: str) -> str:
        """Never disclose a secret through format strings."""
        return str(self)

    def __enter__(self) -> bytes:
        """Reveal bytes only inside the explicit context-manager scope."""
        return self._value

    def __exit__(self, _kind: object, _value: object, _traceback: object) -> None:
        """Close the explicit reveal scope."""

    def use(self) -> "Secret":
        """Return the reveal context manager."""
        return self



@dataclass(frozen=True, slots=True)
class Place:
    """Parsed execution placement declaration."""

    value: str

    @classmethod
    def parse(cls, value: str | "Place") -> "Place":
        """Parse one declared placement without contacting a host."""
        if isinstance(value, cls):
            return value
        if not isinstance(value, str) or not value:
            raise ValueError("place must be a non-empty string")
        return cls(value)


F = TypeVar("F", bound=Callable[..., Any])
def device(name: str, *, family: str, rev: int, place: str | Place = "host") -> Callable[[F], F]:
    """Attach declarative device metadata to a function for standalone tests."""
    parsed = Place.parse(place)
    if not isinstance(name, str) or not name or not isinstance(family, str) or rev < 0:
        raise ValueError("device requires a name, family, and non-negative revision")
    def decorate(function: F) -> F:
        setattr(function, "__omp_device__", {"name": name, "family": family, "rev": rev, "place": parsed})
        return function
    return decorate


EnvPath = PurePosixPath

__all__ = (
    "Capability", "EnvPath", "FailureCode", "OmpError", "PackageError", "Place",
    "Provenance", "Secret", "WarningCode", "WorkspaceUri", "device", "diagnostics",
    "env", "journal", "packages", "ui",
)
