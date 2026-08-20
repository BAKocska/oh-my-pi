"""Read-only package and site-tree metadata for the current omp host.

The host installs a verified snapshot before extension code runs.  This module never
scans ``sys.path``, imports a requested module, opens a lock, or touches the network:
that keeps deployment introspection declarative and preserves isolated-host policy.
"""
from __future__ import annotations

import json
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from types import ModuleType
from typing import Any, Callable, Iterable, Mapping


class PackageError(RuntimeError):
    """Package metadata is unavailable in the current execution context."""


class ResolutionError(PackageError):
    """Verified package ownership metadata contradicts the materialized site tree."""


class IntegrityError(PackageError):
    """An on-demand distribution integrity verification failed."""


class GrantError(PackageError):
    """A deployment operation lacks the operator-recorded capability grant."""


class Origin(StrEnum):
    """How a distribution became visible in this host's site tree."""

    FROZEN = "frozen"
    STORE = "store"
    LINK = "link"


def _normalize(name: str) -> str:
    """Return the PEP 503 comparison form of a distribution name."""
    if not isinstance(name, str) or not name:
        raise TypeError("distribution name must be a non-empty str")
    normalized = "-".join(part for part in name.lower().replace("_", "-").replace(".", "-").split("-") if part)
    if not normalized:
        raise ValueError("distribution name must include an alphanumeric segment")
    return normalized


@dataclass(frozen=True, slots=True)
class Provenance:
    """The structurally stamped provenance septet for an extension action."""

    publisher: str
    extension_id: str
    version: str
    artifact_digest: str
    layer: str
    tier: str
    generation: int


@dataclass(frozen=True, slots=True)
class SiteTree:
    """One host's single materialized import tree."""

    path: Path
    key: str
    layer: str
    tier: str
    pool: str | None
    resolution: str
    lock: Path | None


@dataclass(frozen=True, slots=True)
class Distribution:
    """Verified metadata for one distribution visible to this host."""

    name: str
    version: str
    extension_id: str | None
    origin: Origin
    tag: str | None
    blake3: str | None
    root: Path | None
    files: tuple[Path, ...]
    requested_by: tuple[str, ...]
    vendored: tuple[str, ...]

    def verify(self, deep: bool = False) -> None:
        """Ask the host to verify this distribution's recorded integrity.

        Verification is deliberately explicit: listing metadata stays allocation-only,
        while a security-sensitive extension may request hash or RECORD verification.
        """
        if _verifier is None:
            raise IntegrityError("no package verifier is installed for this host")
        try:
            _verifier(self, deep)
        except IntegrityError:
            raise
        except Exception as error:  # Host bridges use their own concrete error types.
            raise IntegrityError(str(error)) from error


_snapshot: tuple[Distribution, ...] = ()
_by_name: dict[str, Distribution] = {}
_module_owners: dict[str, Distribution] = {}
_own_distribution: Distribution | None = None
_site_tree: SiteTree | None = None
_verifier: Callable[[Distribution, bool], None] | None = None


def _distribution(value: Distribution | Mapping[str, Any]) -> Distribution:
    """Decode one host-supplied, already-verified metadata record."""
    if isinstance(value, Distribution):
        return value
    if not isinstance(value, Mapping):
        raise TypeError("distribution metadata must be a Distribution or mapping")
    origin = value.get("origin", Origin.FROZEN)
    return Distribution(
        name=_normalize(str(value["name"])),
        version=str(value["version"]),
        extension_id=value.get("extension_id"),
        origin=origin if isinstance(origin, Origin) else Origin(str(origin)),
        tag=value.get("tag"),
        blake3=value.get("blake3"),
        root=None if value.get("root") is None else Path(value["root"]),
        files=tuple(Path(path) for path in value.get("files", ())),
        requested_by=tuple(str(item) for item in value.get("requested_by", ())),
        vendored=tuple(str(item) for item in value.get("vendored", ())),
    )


def _install_snapshot(
    distributions: Iterable[Distribution | Mapping[str, Any]],
    *,
    modules: Mapping[str, str | Distribution] = {},
    own: str | Distribution | None = None,
    tree: SiteTree | Mapping[str, Any] | None = None,
    verifier: Callable[[Distribution, bool], None] | None = None,
) -> None:
    """Install the host-generated read snapshot; private to the embedding bridge."""
    global _snapshot, _by_name, _module_owners, _own_distribution, _site_tree, _verifier
    snapshot = tuple(_distribution(item) for item in distributions)
    by_name = {_normalize(item.name): item for item in snapshot}
    if len(by_name) != len(snapshot):
        raise ResolutionError("site snapshot contains duplicate normalized distribution names")
    owners: dict[str, Distribution] = {}
    for module, owner in modules.items():
        resolved = by_name[_normalize(owner)] if isinstance(owner, str) else owner
        if resolved not in snapshot:
            raise ResolutionError(f"module owner {module!r} is not in the site snapshot")
        owners[module] = resolved
    if isinstance(own, str):
        own = by_name.get(_normalize(own))
    if own is not None and own not in snapshot:
        raise ResolutionError("own distribution is not in the site snapshot")
    if isinstance(tree, Mapping):
        tree = SiteTree(
            path=Path(tree["path"]), key=str(tree["key"]), layer=str(tree["layer"]),
            tier=str(tree["tier"]), pool=tree.get("pool"), resolution=str(tree["resolution"]),
            lock=None if tree.get("lock") is None else Path(tree["lock"]),
        )
    _snapshot, _by_name, _module_owners = snapshot, by_name, owners
    _own_distribution, _site_tree, _verifier = own, tree, verifier


def _install_snapshot_json(envelope: bytes | str) -> None:
    """Decode a native bootstrap envelope and install its verified package snapshot.

    The embedding host reads ``OMP_EXT_PACKAGE_SNAPSHOT`` and invokes this private
    bridge before extension code starts.  Keeping environment access outside this
    module preserves zero-I/O import semantics.
    """
    try:
        value = json.loads(envelope)
    except (TypeError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ResolutionError("package snapshot envelope is not valid JSON") from error
    if not isinstance(value, Mapping):
        raise ResolutionError("package snapshot envelope must be an object")
    distributions = value.get("distributions")
    modules = value.get("modules", {})
    if not isinstance(distributions, list) or not isinstance(modules, Mapping):
        raise ResolutionError("package snapshot has invalid distributions or modules")
    _install_snapshot(
        distributions,
        modules=modules,
        own=value.get("own"),
        tree=value.get("tree"),
    )


def list() -> list[Distribution]:
    """Return every distribution visible in this host's site tree."""
    return builtins.list(_snapshot)


def get(name: str) -> Distribution | None:
    """Look up a distribution by its PEP 503 normalized name."""
    return _by_name.get(_normalize(name))


def of(module: str | ModuleType) -> Distribution | None:
    """Return the RECORD owner of a loaded module without importing it."""
    name = module if isinstance(module, str) else module.__name__
    if not isinstance(name, str):
        raise TypeError("module must be a module object or module name")
    while name:
        owner = _module_owners.get(name)
        if owner is not None:
            return owner
        name = name.rpartition(".")[0]
    return None


def own() -> Distribution:
    """Return the calling extension distribution or raise outside extension code."""
    if _own_distribution is None:
        raise PackageError("no calling extension distribution is installed")
    return _own_distribution


def site() -> SiteTree:
    """Return this host's single materialized site tree."""
    if _site_tree is None:
        raise PackageError("no site tree is installed for this host")
    return _site_tree


# Kept as an alias instead of an import-time module alias so the public API can
# remain exactly ``omp.packages.list`` without shadowing Python's list globally.
import builtins

__all__ = (
    "Distribution", "GrantError", "IntegrityError", "Origin", "PackageError",
    "Provenance", "ResolutionError", "SiteTree", "get", "list", "of", "own", "site",
)
