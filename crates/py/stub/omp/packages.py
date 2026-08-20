"""Read-only package metadata types for the standalone omp stub."""
from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from types import ModuleType


class Origin(StrEnum):
    """How a distribution is made visible to an omp host."""

    FROZEN = "frozen"
    STORE = "store"
    LINK = "link"


@dataclass(frozen=True, slots=True)
class SiteTree:
    """A single materialized import tree."""

    path: Path
    key: str
    layer: str
    tier: str
    pool: str | None
    resolution: str
    lock: Path | None


@dataclass(frozen=True, slots=True)
class Distribution:
    """Read-only distribution record usable in standalone assertions."""

    name: str
    version: str
    extension_id: str | None = None
    origin: Origin = Origin.FROZEN
    tag: str | None = None
    blake3: str | None = None
    root: Path | None = None
    files: tuple[Path, ...] = ()
    requested_by: tuple[str, ...] = ()
    vendored: tuple[str, ...] = ()

    def verify(self, deep: bool = False) -> None:
        """Accept the caller-created fake record without touching the filesystem."""
        del deep


_records: tuple[Distribution, ...] = ()
_tree: SiteTree | None = None
_own: Distribution | None = None


def install(records: tuple[Distribution, ...], *, tree: SiteTree | None = None, own: Distribution | None = None) -> None:
    """Install deterministic metadata for one standalone test."""
    global _records, _tree, _own
    _records, _tree, _own = tuple(records), tree, own


def list() -> list[Distribution]:
    """Return installed fake distribution metadata."""
    return [*_records]


def get(name: str) -> Distribution | None:
    """Look up a fake distribution by PEP 503-normalized name."""
    normalized = name.lower().replace("_", "-").replace(".", "-")
    return next((item for item in _records if item.name.lower().replace("_", "-").replace(".", "-") == normalized), None)


def of(module: str | ModuleType) -> Distribution | None:
    """Return no owner because the stub does not synthesize RECORD files."""
    del module
    return None


def own() -> Distribution:
    """Return the installed extension record or raise outside a fake host."""
    if _own is None:
        raise RuntimeError("no calling extension distribution is installed")
    return _own


def site() -> SiteTree:
    """Return the installed fake site tree or raise when absent."""
    if _tree is None:
        raise RuntimeError("no site tree is installed")
    return _tree


__all__ = ("Distribution", "Origin", "SiteTree", "get", "install", "list", "of", "own", "site")
