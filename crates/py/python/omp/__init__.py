"""The frozen omp Python extension API.

The package is declarative at import time: importing it performs no filesystem,
network, subprocess, Environment, CONTROL, or DATA operation.
"""

from __future__ import annotations

import asyncio as _asyncio
import contextvars as _contextvars
import inspect as _inspect
from typing import Any as _Any

from _omp import (
    ActivateReason,
    AgentUrl,
    ApiLevelError,
    ArtifactUrl,
    Authority,
    BlobRef,
    CapabilityError,
    ClientPath,
    CostClass,
    DeadlineExceeded,
    DeclarationLimit,
    DeclarationSealed,
    DuplicateRegistration,
    Durability,
    Duration,
    EffectsNotAuthorized,
    EnvPath,
    EnvUnavailable,
    FrameTooLarge,
    HistoryUrl,
    HostDisconnected,
    InvocationPhase,
    LifecyclePhase,
    ManifestError,
    OmpError,
    OperationSpec,
    RestartReason,
    StateScope,
    PlacementError,
    Principal,
    QuotaExceeded,
    ResourceReceipt,
    StaleGeneration,
    TrustError,
    WorkspaceUri,
    _scheme_snapshot,
    _phase_legality_matrix,
    _runtime_metadata,
    operation_spec as _native_operation_spec,
)


class Fault:
    """Marker base for a device's durable typed failure value."""

    __slots__ = ()

    def __new__(cls, *_args: _Any, **_kwargs: _Any) -> Fault:
        if cls is Fault:
            raise TypeError("Fault is a marker base; instantiate a frozen dataclass subclass")
        return super().__new__(cls)




class StateScopeDenied(OmpError):
    """The authenticated principal may not access a requested state scope."""

_control_backend: _contextvars.ContextVar[_Any | None] = _contextvars.ContextVar(
    "omp_control_backend", default=None
)


def _install_control_backend(backend: _Any) -> None:
    """Install the host-owned CONTROL bridge in the active invocation context."""
    _control_backend.set(backend)

async def _control_request(operation: str, /, **arguments: _Any) -> _Any:
    backend = _control_backend.get()
    if backend is None:
        raise HostDisconnected("no CONTROL request bridge is installed")
    request = backend.request
    if _inspect.iscoroutinefunction(request):
        return await request(operation, arguments)
    result = await _asyncio.to_thread(request, operation, arguments)
    if _inspect.isawaitable(result):
        return await result
    return result


async def _read_url(url: _Any) -> _Any:
    """Resolve a typed URL through the host CONTROL resolver."""
    return await _control_request("omp.urls.read", url=url)



class _State:
    """Typed append-log and content-addressed state surface."""

    async def append(
        self,
        entry: _Any,
        *,
        scope: StateScope,
        idempotency_key: str | None = None,
    ) -> _Any:
        """Append one typed state entry durably."""
        return await _control_request(
            "omp.state.append", entry=entry, scope=scope, idempotency_key=idempotency_key
        )

    async def entries(
        self,
        kind: _Any,
        *,
        scope: StateScope,
        since: _Any = None,
        limit: int | None = None,
    ) -> _Any:
        """Read ordered entries of one registered kind."""
        return await _control_request(
            "omp.state.entries", kind=kind, scope=scope, since=since, limit=limit
        )

    async def latest(self, kind: _Any, *, scope: StateScope) -> _Any:
        """Return the latest entry of one kind, if present."""
        return await _control_request("omp.state.latest", kind=kind, scope=scope)

    async def fold(
        self,
        kind: _Any,
        reducer: _Any,
        initial: _Any,
        *,
        scope: StateScope,
        since: _Any = None,
    ) -> tuple[_Any, _Any]:
        """Fold ordered state entries without exposing storage internals."""
        value = initial
        mark = None
        for record in await self.entries(kind, scope=scope, since=since):
            value = reducer(value, record)
            mark = getattr(record, "id", None)
        return value, mark

    async def cas_put(self, data: bytes, *, scope: StateScope) -> BlobRef:
        """Store content-addressed state rooted in a durable scope."""
        return await _control_request("omp.state.cas_put", data=data, scope=scope)

    async def cas_get(self, ref: BlobRef, *, scope: StateScope) -> bytes:
        """Read content-addressed state rooted in a durable scope."""
        return await _control_request("omp.state.cas_get", ref=ref, scope=scope)


def operation_spec(symbol: str | _Any) -> OperationSpec | None:
    """Return canonical generated operation metadata for a public symbol."""
    return _native_operation_spec(symbol)


async def state_dir() -> EnvPath:
    """Return the Environment path for rebuildable extension indices."""
    return await _control_request("omp.state_dir")


state = _State()
CancelledError = _asyncio.CancelledError

# Importing these frozen modules only creates declarations and namespace values.
from . import env as env
from . import urls as urls
from .urls import (
    Scheme,
    SchemeInfo,
    SchemeNotReadable,
    Selector,
    SelectorError,
    Url,
    UrlError,
    parse,
    parse_selector,
    schemes,
)
from ._registry import (
    DeclarationDrift,
    DeclarationRegistry,
    MAX_DECLARATIONS,
    DeclarationSnapshot,
    QuotaStatus,
    ResourceReceipt,
    ServiceClient,
    ServiceDefinition,
    Services,
    service,
    services,
    resources,
)
urls._bind_scheme_source(_scheme_snapshot)

RUNTIME_METADATA = _runtime_metadata()
PHASE_LEGALITY_MATRIX = _phase_legality_matrix()


def _attach_generated_metadata() -> None:
    namespace = globals()
    for public_name, metadata in RUNTIME_METADATA.items():
        parts = public_name.split(".")
        if not parts or parts[0] != "omp":
            continue
        target: _Any = namespace.get(parts[1])
        for part in parts[2:]:
            if target is None:
                break
            target = getattr(target, part, None)
        if target is None:
            continue
        target = getattr(target, "__func__", target)
        try:
            target.__omp_symbol__ = public_name
            target.__operation_spec__ = metadata["operation"]
            target.__signature_text__ = metadata["signature"]
            target.__examples__ = tuple(metadata["examples"])
            target.__owner__ = metadata["owner"]
        except (AttributeError, TypeError):
            # Native immutable classes expose metadata through RUNTIME_METADATA.
            pass


_attach_generated_metadata()
del _attach_generated_metadata



__all__ = (
    "ActivateReason",
    "AgentUrl",
    "ApiLevelError",
    "ArtifactUrl",
    "Authority",
    "BlobRef",
    "CancelledError",
    "CapabilityError",
    "ClientPath",
    "CostClass",
    "DeadlineExceeded",
    "DeclarationDrift",
    "DeclarationLimit",
    "DeclarationRegistry",
    "DeclarationSealed",
    "DeclarationSnapshot",
    "DuplicateRegistration",
    "Durability",
    "Duration",
    "EffectsNotAuthorized",
    "EnvPath",
    "EnvUnavailable",
    "Fault",
    "FrameTooLarge",
    "HistoryUrl",
    "HostDisconnected",
    "InvocationPhase",
    "PHASE_LEGALITY_MATRIX",
    "LifecyclePhase",
    "ManifestError",
    "OmpError",
    "OperationSpec",
    "MAX_DECLARATIONS",
    "PlacementError",
    "Principal",
    "QuotaExceeded",
    "RUNTIME_METADATA",
    "RestartReason",
    "Scheme",
    "SchemeInfo",
    "SchemeNotReadable",
    "Selector",
    "SelectorError",
    "QuotaStatus",
    "ResourceReceipt",
    "ServiceClient",
    "ServiceDefinition",
    "Services",
    "StaleGeneration",
    "StateScope",
    "StateScopeDenied",
    "Url",
    "UrlError",
    "TrustError",
    "WorkspaceUri",
    "env",
    "operation_spec",
    "resources",
    "service",
    "parse",
    "parse_selector",
    "services",
    "state",
    "state_dir",
    "urls",
    "schemes",
)
