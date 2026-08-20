"""Declared dynamic-device parents and post-FREEZE runtime leaves.

The static declaration table records only a parent with
``devices.parent(name, family=..., rev=..., place=...)`` during IMPORT.  After
FREEZE, an activation hook may attach discovered leaves beneath that authority
with one ``await parent.mount_many(MountSpec(...), ...)`` call.  A mount spec's
``subpath`` is relative to the parent, so runtime discovery cannot claim an
unrelated top-level name or change the parent's family, revision, placement, or
manifest provenance.

Mounting and availability are deliberately distinct.  Mounting supplies stable
identity, schema, docs, and a body for a discovered leaf.  Reachability changes
are sent through ``await devices.set_availability(*deltas)``; all deltas in the
call form one host transition, one catalog notification, and one journal item.
``enable`` and ``disable`` are convenience batch constructors over that same
operation.  Thus dynamic MCP leaves use the ordinary ``omp.devices`` mounted
set rather than maintaining a second registry or re-registering on reconnect.
The Part 2 host arm is not present in this freeze half, so every post-FREEZE
method raises :class:`omp.NotWiredError` at CALL time.
"""
from __future__ import annotations

from collections.abc import Callable, Mapping
from dataclasses import dataclass
from types import MappingProxyType
from typing import Any

from ._errors import NotWiredError
from ._registry import registry


def _segment(value: str) -> str:
    if not isinstance(value, str) or not value:
        raise ValueError("device path segments must be non-empty strings")
    if not value[0].isalpha() or not value[0].islower() or len(value) > 64:
        raise ValueError(f"invalid device path segment {value!r}")
    if any(not (character.islower() or character.isdigit() or character == "_") for character in value):
        raise ValueError(f"invalid device path segment {value!r}")
    return value


def _subpath(value: str) -> str:
    if not isinstance(value, str):
        raise TypeError("dynamic device subpath must be a string")
    parts = value.split("/")
    if not parts or any(not part for part in parts):
        raise ValueError(f"invalid dynamic device subpath {value!r}")
    return "/".join(_segment(part) for part in parts)


@dataclass(frozen=True, slots=True)
class MountSpec:
    """One runtime-discovered leaf relative to a declared parent."""

    subpath: str
    body: Callable[..., Any]
    schema: Mapping[str, object]
    summary: str
    docs: str | None = None

    def __post_init__(self) -> None:
        object.__setattr__(self, "subpath", _subpath(self.subpath))
        if not callable(self.body):
            raise TypeError("dynamic device body must be callable")
        if not isinstance(self.schema, Mapping):
            raise TypeError("dynamic device schema must be a mapping")
        object.__setattr__(self, "schema", MappingProxyType(dict(self.schema)))
        if not isinstance(self.summary, str):
            raise TypeError("dynamic device summary must be a string")


@dataclass(frozen=True, slots=True)
class AvailabilityDelta:
    """One desired mountedness change in a batched availability transition."""

    path: str
    mounted: bool
    reason: str | None = None

    def __post_init__(self) -> None:
        if not isinstance(self.path, str) or not self.path:
            raise ValueError("availability path must be a non-empty string")
        if not isinstance(self.mounted, bool):
            raise TypeError("availability mounted must be bool")
        if self.mounted and self.reason is not None:
            raise ValueError("an available device cannot carry an unavailable reason")


@dataclass(frozen=True, slots=True)
class DynamicDeviceParent:
    """Manifest-authorized top-level parent for post-FREEZE discovered leaves."""

    name: str
    family: str
    rev: int
    place: str

    def path(self, subpath: str) -> str:
        """Return the canonical absolute path for one relative leaf."""
        return f"{self.name}/{_subpath(subpath)}"

    async def mount_many(self, *specs: MountSpec) -> tuple[str, ...]:
        """Mount discovered leaves in one runtime request.

        The Part 2 host arm owns atomic validation and dispatch installation.
        """
        del specs
        raise NotWiredError("omp.devices.dynamic_mount")

    async def mount(self, spec: MountSpec) -> str:
        """Mount one discovered leaf through the same runtime operation."""
        del spec
        raise NotWiredError("omp.devices.dynamic_mount")


class Devices:
    """Static parent declarations plus the session-scoped mounted-set surface."""

    __slots__ = ()

    def parent(
        self,
        name: str,
        *,
        family: str,
        rev: int,
        place: str = "host",
    ) -> DynamicDeviceParent:
        """Declare a manifest-backed dynamic parent during IMPORT."""
        parent = DynamicDeviceParent(_segment(name), family, rev, place)
        registry.register_tool(parent.name, parent.family, parent.rev, parent)
        return parent

    async def set_availability(self, *deltas: AvailabilityDelta) -> None:
        """Apply one atomic mounted-set transition for all supplied deltas."""
        del deltas
        raise NotWiredError("omp.devices.set_availability")

    async def enable(self, *paths: str) -> None:
        """Enable paths as one availability transition."""
        del paths
        raise NotWiredError("omp.devices.set_availability")

    async def disable(self, *paths: str, reason: str | None = None) -> None:
        """Disable paths as one availability transition with one reason."""
        del paths, reason
        raise NotWiredError("omp.devices.set_availability")

    async def refresh(self) -> tuple[object, ...]:
        """Recompute ordinary availability predicates as one transition."""
        raise NotWiredError("omp.devices.refresh")


# One namespace instance ensures every dynamic path uses the ordinary mounted set.
devices = Devices()

__all__ = (
    "AvailabilityDelta",
    "Devices",
    "DynamicDeviceParent",
    "MountSpec",
    "devices",
)
