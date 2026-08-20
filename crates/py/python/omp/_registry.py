"""Frozen extension declarations and manifest-gated CONTROL services.

Importing this module performs no I/O and does not open either host socket. The
host installs its existing CONTROL request transport only after declaration
verification; journal entries and agent messages are never accepted as service
transports.
"""

from __future__ import annotations

import inspect
from collections.abc import Awaitable, Callable, Iterable, Mapping
from dataclasses import dataclass
from typing import Protocol, TypeVar

from _omp import (
    CapabilityError,
    DeclarationLimit,
    DeclarationSealed,
    DuplicateRegistration,
    QuotaExceeded,
    QuotaStatus,
    ResourceReceipt,
    resources,
)


_T = TypeVar("_T", bound=type)
_ToolKey = tuple[str, str, int]
_HookKey = tuple[str, str]
_ServiceKey = tuple[str, int]

MAX_DECLARATIONS = 256
"""Maximum decorator declarations accepted from one extension."""


class DeclarationDrift(RuntimeError):
    """The frozen decorator existence sets differ from the manifest."""

    def __init__(
        self,
        *,
        missing_tools: frozenset[_ToolKey],
        unexpected_tools: frozenset[_ToolKey],
        missing_hooks: frozenset[_HookKey],
        unexpected_hooks: frozenset[_HookKey],
        missing_services: frozenset[_ServiceKey],
        unexpected_services: frozenset[_ServiceKey],
    ) -> None:
        super().__init__("frozen declarations differ from the manifest")
        self.missing_tools = missing_tools
        self.unexpected_tools = unexpected_tools
        self.missing_hooks = missing_hooks
        self.unexpected_hooks = unexpected_hooks
        self.missing_services = missing_services
        self.unexpected_services = unexpected_services


@dataclass(frozen=True, slots=True)
class ServiceDefinition:
    """One sealed ``@omp.service`` implementation."""

    name: str
    rev: int
    implementation: type
    methods: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class DeclarationSnapshot:
    """Immutable view of the complete decorator registry."""

    tools: frozenset[_ToolKey]
    hooks: frozenset[_HookKey]
    services: frozenset[_ServiceKey]


class DeclarationRegistry:
    """Process-local declaration authority sealed exactly once at FREEZE."""

    __slots__ = (
        "_configured",
        "_hooks",
        "_manifest_hooks",
        "_manifest_requires",
        "_manifest_services",
        "_manifest_tools",
        "_sealed",
        "_service_instances",
        "_services",
        "_tools",
        "_verified",
    )

    def __init__(self) -> None:
        self._configured = False
        self._sealed = False
        self._verified = False
        self._tools: dict[_ToolKey, object] = {}
        self._hooks: dict[_HookKey, object] = {}
        self._services: dict[_ServiceKey, ServiceDefinition] = {}
        self._service_instances: dict[_ServiceKey, object] = {}
        self._manifest_tools: frozenset[_ToolKey] = frozenset()
        self._manifest_hooks: frozenset[_HookKey] = frozenset()
        self._manifest_services: frozenset[_ServiceKey] = frozenset()
        self._manifest_requires: frozenset[_ServiceKey] = frozenset()

    @property
    def sealed(self) -> bool:
        """Whether FREEZE has made every declaration immutable."""

        return self._sealed

    @property
    def required_services(self) -> frozenset[_ServiceKey]:
        """Manifest-granted service dependencies for this extension."""

        return self._manifest_requires

    def configure_manifest(
        self,
        *,
        tools: Iterable[_ToolKey] = (),
        hooks: Iterable[_HookKey] = (),
        services: Iterable[_ServiceKey] = (),
        requires: Iterable[_ServiceKey] = (),
    ) -> None:
        """Installs authoritative manifest sets before the first module import."""

        self._ensure_open()
        if self._configured:
            raise RuntimeError("manifest declaration sets are already configured")
        if self._tools or self._hooks or self._services:
            raise RuntimeError("manifest must be configured before declaration import")
        self._manifest_tools = frozenset(_tool_key(*item) for item in tools)
        self._manifest_hooks = frozenset(_hook_key(*item) for item in hooks)
        self._manifest_services = frozenset(_service_key(*item) for item in services)
        self._manifest_requires = frozenset(_service_key(*item) for item in requires)
        self._configured = True

    def register_tool(
        self,
        name: str,
        family: str,
        rev: int,
        declaration: object,
    ) -> object:
        """Records a tool decorator during sequential manifest import."""

        key = _tool_key(name, family, rev)
        self._insert(self._tools, key, declaration, "tool")
        return declaration

    def register_hook(self, event: str, phase: object, handler: object) -> object:
        """Records a hook decorator during sequential manifest import."""

        key = _hook_key(event, phase)
        self._insert(self._hooks, key, handler, "hook")
        return handler

    def register_service(self, name: str, rev: int, implementation: type) -> type:
        """Records and validates an async service implementation."""

        key = _service_key(name, rev)
        if not isinstance(implementation, type):
            raise TypeError("@omp.service may decorate only a class")
        members = inspect.getmembers(implementation)
        methods = tuple(
            method_name
            for method_name, value in members
            if not method_name.startswith("_") and inspect.iscoroutinefunction(value)
        )
        public_non_async = tuple(
            method_name
            for method_name, value in members
            if not method_name.startswith("_")
            and callable(value)
            and not inspect.iscoroutinefunction(value)
        )
        if public_non_async:
            names = ", ".join(public_non_async)
            raise TypeError(f"service public methods must be async: {names}")
        if not methods:
            raise TypeError("a service must declare at least one public async method")
        definition = ServiceDefinition(key[0], key[1], implementation, methods)
        self._insert(self._services, key, definition, "service")
        return implementation

    def freeze(self) -> DeclarationSnapshot:
        """Seals the Core-verified registry and returns its immutable sets."""

        self._sealed = True
        self._verified = True
        return self.snapshot()

    def snapshot(self) -> DeclarationSnapshot:
        """Returns the current declaration existence sets without mutation."""

        return DeclarationSnapshot(
            tools=frozenset(self._tools),
            hooks=frozenset(self._hooks),
            services=frozenset(self._services),
        )


    def service_definition(self, name: str, rev: int) -> ServiceDefinition:
        """Returns one verified provider definition for CONTROL dispatch."""

        if not self._verified:
            raise RuntimeError("service dispatch is unavailable before FREEZE")
        key = _service_key(name, rev)
        try:
            return self._services[key]
        except KeyError as error:
            raise LookupError(f"service {name!r} rev {rev} is not registered") from error

    def service_instance(self, name: str, rev: int) -> object:
        """Returns the generation-local provider instance for a verified service."""

        definition = self.service_definition(name, rev)
        key = (definition.name, definition.rev)
        instance = self._service_instances.get(key)
        if instance is None:
            instance = definition.implementation()
            self._service_instances[key] = instance
        return instance

    def _ensure_open(self) -> None:
        if self._sealed:
            raise DeclarationSealed("declaration registry is sealed")

    def _insert(
        self,
        declarations: dict[object, object],
        key: object,
        value: object,
        kind: str,
    ) -> None:
        self._ensure_open()
        if len(self._tools) + len(self._hooks) + len(self._services) >= MAX_DECLARATIONS:
            raise DeclarationLimit(
                f"extension exceeds the {MAX_DECLARATIONS} declaration limit"
            )
        if key in declarations:
            raise DuplicateRegistration(f"duplicate {kind} declaration: {key!r}")
        declarations[key] = value


class ControlServiceTransport(Protocol):
    """Existing host CONTROL request path used by service clients."""

    def request(self, operation: str, payload: Mapping[str, object]) -> Awaitable[object]:
        """Sends one correlated Request and awaits its matching response."""


class ServiceClient:
    """Dynamic typed-service proxy bound to an exact name and revision."""

    __slots__ = ("_name", "_rev", "_transport")

    def __init__(self, name: str, rev: int, transport: ControlServiceTransport) -> None:
        self._name = name
        self._rev = rev
        self._transport = transport

    @property
    def name(self) -> str:
        """Globally qualified service name."""

        return self._name

    @property
    def rev(self) -> int:
        """Exact service revision."""

        return self._rev

    def __getattr__(self, method: str) -> Callable[..., Awaitable[object]]:
        if method.startswith("_"):
            raise AttributeError(method)

        async def invoke(*args: object, **kwargs: object) -> object:
            return await self._transport.request(
                "service.call",
                {
                    "name": self._name,
                    "rev": self._rev,
                    "method": method,
                    "args": args,
                    "kwargs": kwargs,
                },
            )

        return invoke


class Services:
    """Manifest-gated service connector using only the CONTROL request path."""

    __slots__ = ("_transport",)

    def __init__(self) -> None:
        self._transport: ControlServiceTransport | None = None

    def _install_control_transport(self, transport: ControlServiceTransport) -> None:
        """Installs the host's correlated CONTROL transport after VERIFY."""

        if self._transport is not None and self._transport is not transport:
            raise RuntimeError("CONTROL service transport is already installed")
        self._transport = transport

    async def connect(self, name: str, *, rev: int) -> ServiceClient:
        """Connects to an exact service revision granted by ``[requires]``."""

        key = _service_key(name, rev)
        if key not in registry.required_services:
            raise CapabilityError(
                f"manifest does not grant service dependency {name!r} rev {rev}"
            )
        transport = self._transport
        if transport is None:
            raise RuntimeError("CONTROL service transport is unavailable before ACTIVATE")
        await transport.request("service.connect", {"name": key[0], "rev": key[1]})
        return ServiceClient(key[0], key[1], transport)


registry = DeclarationRegistry()
"""The sole declaration authority in one extension-host process."""

services = Services()
"""The sole manifest-gated service connector in one extension-host process."""


def configure_manifest(
    *,
    tools: Iterable[_ToolKey] = (),
    hooks: Iterable[_HookKey] = (),
    services: Iterable[_ServiceKey] = (),
    requires: Iterable[_ServiceKey] = (),
) -> None:
    """Installs authoritative existence sets before sequential import."""

    registry.configure_manifest(
        tools=tools,
        hooks=hooks,
        services=services,
        requires=requires,
    )


def freeze_declarations() -> DeclarationSnapshot:
    """Runs the FREEZE transition without socket or filesystem work."""

    return registry.freeze()




def service(name: str, *, rev: int) -> Callable[[_T], _T]:
    """Declares an async inter-extension service implementation."""

    key = _service_key(name, rev)

    def decorate(implementation: _T) -> _T:
        registry.register_service(key[0], key[1], implementation)
        return implementation

    return decorate


async def dispatch_service(
    request_id: int,
    name: str,
    rev: int,
    method: str,
    args: tuple[object, ...],
    kwargs: Mapping[str, object],
) -> tuple[int, object]:
    """Dispatches a correlated provider call received from CONTROL."""

    if isinstance(request_id, bool) or not isinstance(request_id, int) or request_id <= 0:
        raise ValueError("service request correlation id must be a positive integer")
    definition = registry.service_definition(name, rev)
    if method not in definition.methods:
        raise AttributeError(f"service {name!r} has no public async method {method!r}")
    instance = registry.service_instance(name, rev)
    result = await getattr(instance, method)(*args, **dict(kwargs))
    return request_id, result




def _tool_key(name: str, family: str, rev: int) -> _ToolKey:
    if not name:
        raise ValueError("tool name must be non-empty")
    if isinstance(rev, bool) or not isinstance(rev, int) or not 0 <= rev <= 65_535:
        raise ValueError("tool rev must be an unsigned 16-bit integer")
    return name, family, rev


def _hook_key(event: str, phase: object) -> _HookKey:
    if not event:
        raise ValueError("hook event must be non-empty")
    value = getattr(phase, "value", phase)
    if not isinstance(value, str) or not value:
        raise ValueError("hook phase must be a non-empty string enum")
    return event, value.lower()


def _service_key(name: str, rev: int) -> _ServiceKey:
    if not name or "." not in name:
        raise ValueError("service name must be globally qualified")
    if (
        isinstance(rev, bool)
        or not isinstance(rev, int)
        or not 1 <= rev <= 4_294_967_295
    ):
        raise ValueError("service rev must be a positive unsigned 32-bit integer")
    return name, rev


__all__ = (
    "ControlServiceTransport",
    "DeclarationDrift",
    "DeclarationRegistry",
    "DeclarationSnapshot",
    "MAX_DECLARATIONS",
    "QuotaExceeded",
    "QuotaStatus",
    "ResourceReceipt",
    "ServiceClient",
    "ServiceDefinition",
    "Services",
    "configure_manifest",
    "dispatch_service",
    "freeze_declarations",
    "registry",
    "resources",
    "service",
    "services",
)
