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
from enum import IntEnum, StrEnum
from types import MappingProxyType
from typing import Any

from ._errors import ExtensionError, NotWiredError
from ._registry import registry
from .packages import Provenance
from .placement import Place
from .policy import Tier


EXTERNAL_SUMMARY_CAP = 200
PER_DEVICE_CAP = 10_000


class Precedence(IntEnum):
    """Order competing claims on one device name."""

    CORE = 1000
    INTEGRATION = 700
    ENHANCEMENT = 500
    DEFAULT = 0
    FALLBACK = -500


@dataclass(frozen=True, slots=True)
class Availability:
    """Describe whether a declared device is currently mounted."""

    mounted: bool
    reason: str | None = None


@dataclass(frozen=True, slots=True)
class Example:
    """Describe one worked device invocation."""

    args: Mapping[str, object]
    note: str | None = None
    result: str | None = None


@dataclass(frozen=True, slots=True)
class DocEffects:
    """Bound a device's document effects."""

    read: bool = False
    write_globs: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class ExecEffects:
    """Bound a device's command and network effects."""

    commands: tuple[str, ...] = ()
    network: bool = False


@dataclass(frozen=True, slots=True)
class InferenceEffects:
    """Bound a device's inference effects."""

    max_requests: int = 0
    max_usd: float = 0.0


@dataclass(frozen=True, slots=True)
class Effects:
    """Declare a device's maximum static effect envelope."""

    documents: DocEffects | None = None
    exec: ExecEffects | None = None
    inference: InferenceEffects | None = None
    subagents: int = 0


class DocsMode(StrEnum):
    """Select how much device documentation is inlined."""

    CATALOG = "catalog"
    BUILTINS = "builtins"
    INLINE = "inline"


class DeviceError(ExtensionError):
    """A device declaration or runtime operation failed."""


class DeviceNameError(DeviceError):
    """Raised when a device name or precedence claim is invalid."""


class SchemaError(DeviceError):
    """Raised when a device schema or example is invalid."""


class PrecedenceConflict(DeviceError):
    """Raised when device claims cannot be ordered unambiguously."""


class DocsBudgetError(DeviceError):
    """Raised when device documentation exceeds its allowed budget."""


class DeviceUnavailable(DeviceError):
    """Raised when no mounted device satisfies a requested path."""


def _segment(value: str) -> str:
    if not isinstance(value, str) or not value:
        raise ValueError("device path segments must be non-empty strings")
    if (
        len(value) > 64
        or not ("a" <= value[0] <= "z")
        or any(
            not ("a" <= character <= "z" or "0" <= character <= "9" or character == "_")
            for character in value
        )
    ):
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
class ToolPath:
    """Identify one device-tree path and optional claimant."""

    name: str
    sub: str | None = None
    claimant: str | None = None

    def __post_init__(self) -> None:
        try:
            _segment(self.name)
            if self.sub is not None:
                _subpath(self.sub)
        except (TypeError, ValueError) as error:
            raise DeviceError(str(error)) from error
        if self.claimant is not None:
            if (
                not isinstance(self.claimant, str)
                or self.claimant.count("/") != 1
                or any(not part for part in self.claimant.split("/"))
            ):
                raise DeviceError(f"invalid device claimant {self.claimant!r}")

    def __str__(self) -> str:
        """Render the canonical model-facing device path."""
        rendered = self.name
        if self.sub is not None:
            rendered = f"{rendered}/{self.sub}"
        if self.claimant is not None:
            rendered = f"{rendered}@{self.claimant}"
        return rendered


@dataclass(frozen=True, slots=True)
class DeviceInfo:
    """Capture an immutable snapshot of one device claimant."""

    name: str
    family: str
    rev: int
    identity: str
    claimant: str
    path: ToolPath
    summary: str | None
    place: Place
    precedence: int
    tier: Tier
    effects: Effects | None
    mounted: bool
    enabled: bool
    available: bool
    reason: str | None
    shadowed_by: str | None
    source: str
    provenance: Provenance
    slotted: bool
    schema_bytes: int
    schema_tokens: int


class Device:
    """Provide the live handle returned by ``@omp.device``."""

    __slots__ = (
        "body",
        "docs",
        "enabled",
        "family",
        "identity",
        "mounted",
        "name",
        "path",
        "place",
        "precedence",
        "replaces",
        "rev",
        "schema",
        "shadowed_by",
        "shadows",
        "summary",
    )

    def __init__(
        self,
        *,
        name: str,
        family: str,
        rev: int,
        place: Place,
        precedence: int,
        replaces: str | None,
        schema: type | dict[str, object] | None,
        docs: object | None,
        summary: str | None,
        body: Callable[..., Any],
    ) -> None:
        """Initialize one declared device handle."""
        self.name = name
        self.family = family
        self.rev = rev
        self.identity = f"{name}@{family or name}/{rev}"
        self.path = ToolPath(name)
        self.place = place
        self.precedence = precedence
        self.replaces = replaces
        self.schema = schema
        self.docs = docs
        self.summary = summary
        self.body = body
        self.enabled = True
        self.mounted = False
        self.shadows = ()
        self.shadowed_by = None

    def enable(self) -> None:
        """Enable this device for the host's FREEZE projection."""
        registry.set_device_enabled(self.name, self.family, self.rev, True)
        self.enabled = True

    def disable(self, reason: str | None = None) -> None:
        """Disable this device for the host's FREEZE projection."""
        registry.set_device_enabled(
            self.name, self.family, self.rev, False, reason=reason
        )
        self.enabled = False

    def subtool(self, name: str) -> ToolPath:
        """Return a typed child path below this device."""
        try:
            sub = _subpath(name)
        except (TypeError, ValueError) as error:
            raise DeviceError(str(error)) from error
        return ToolPath(self.name, sub)

    async def __call__(self, *args: Any, **kwargs: Any) -> Any:
        """Invoke the decorated body directly in process."""
        return await self.body(*args, **kwargs)


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

    EXTERNAL_SUMMARY_CAP = EXTERNAL_SUMMARY_CAP
    PER_DEVICE_CAP = PER_DEVICE_CAP

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

    async def list(self, *, mounted_only: bool = True) -> tuple[DeviceInfo, ...]:
        """Return immutable catalog rows, optionally including unmounted claims."""
        del mounted_only
        raise NotWiredError("omp.devices.list")


# One namespace instance ensures every dynamic path uses the ordinary mounted set.
devices = Devices()

__all__ = (
    "Availability",
    "AvailabilityDelta",
    "Device",
    "DeviceError",
    "DeviceInfo",
    "DeviceNameError",
    "DeviceUnavailable",
    "Devices",
    "DocsBudgetError",
    "DocsMode",
    "DocEffects",
    "DynamicDeviceParent",
    "EXTERNAL_SUMMARY_CAP",
    "Effects",
    "Example",
    "ExecEffects",
    "InferenceEffects",
    "MountSpec",
    "PER_DEVICE_CAP",
    "Precedence",
    "PrecedenceConflict",
    "SchemaError",
    "ToolPath",
    "devices",
)
