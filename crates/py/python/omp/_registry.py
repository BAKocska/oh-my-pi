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
_EntryKindKey = tuple[str, str]
_ServiceKey = tuple[str, int]
_ProviderKey = str
_WorkerKey = str

MAX_DECLARATIONS = 256
"""Maximum decorator declarations accepted from one extension."""

@dataclass(frozen=True, slots=True)
class ManifestTableSchema:
    """Ratified authoring spelling for one projected manifest table."""

    table: str
    fields: frozenset[str]


TELEMETRY_MANIFEST_SCHEMA = ManifestTableSchema(
    table="telemetry",
    fields=frozenset({"kinds", "scope", "queue", "overflow"}),
)
"""The ratified ``[[telemetry]]`` authoring row and its required fields."""

SCHEDULES_PROJECT_CAPABILITY = "schedules:project"
"""The ratified capability key granting project-scoped schedules."""


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
class EntryKindDefinition:
    """One import-time ``@omp.entry_kind`` declaration."""

    name: str
    rev: str
    display: bool | None
    spill: bool
    implementation: type
@dataclass(frozen=True, slots=True)
class ProviderDefinition:
    """One import-time ``@omp.provider`` declaration."""

    id: str
    spec: object
    implementation: type


@dataclass(frozen=True, slots=True)
class WorkerDefinition:
    """One import-time ``omp.workers.declare`` declaration."""

    name: str
    spec: object



@dataclass(frozen=True, slots=True)
class TelemetryDefinition:
    """One import-time ``@omp.telemetry`` subscription declaration."""

    kinds: tuple[str, ...]
    scope: str
    queue: int
    overflow: str
    batch: int | None
    replay: bool
    replay_limit: int
    handler: object


@dataclass(frozen=True, slots=True)
class PromptSlotDefinition:
    """One import-time ``@omp.prompt_slot`` contribution declaration."""

    slot: str
    priority: int
    cls: str
    renderer: object





@dataclass(frozen=True, slots=True)
class DeclarationSnapshot:
    """Immutable view of the complete decorator registry."""

    entry_kinds: tuple[EntryKindDefinition, ...]
    tools: frozenset[_ToolKey]
    hooks: frozenset[_HookKey]
    services: frozenset[_ServiceKey]
    telemetry: tuple[TelemetryDefinition, ...] = ()
    prompt_slots: tuple[PromptSlotDefinition, ...] = ()
    providers: tuple[ProviderDefinition, ...] = ()
    workers: tuple[WorkerDefinition, ...] = ()


class DeclarationRegistry:
    """Process-local declaration authority sealed exactly once at FREEZE."""

    __slots__ = (
        "_configured",
        "_entry_kinds",
        "_providers",
        "_hooks",
        "_prompt_slots",
        "_telemetry",
        "_manifest_hooks",
        "_manifest_requires",
        "_manifest_services",
        "_manifest_tools",
        "_sealed",
        "_service_instances",
        "_services",
        "_tools",
        "_workers",
        "_verified",
    )

    def __init__(self) -> None:
        self._configured = False
        self._sealed = False
        self._verified = False
        self._tools: dict[_ToolKey, object] = {}
        self._entry_kinds: dict[_EntryKindKey, EntryKindDefinition] = {}
        self._providers: dict[_ProviderKey, ProviderDefinition] = {}
        self._hooks: dict[_HookKey, object] = {}
        self._telemetry: dict[str, TelemetryDefinition] = {}
        self._prompt_slots: dict[tuple[str, str], PromptSlotDefinition] = {}
        self._services: dict[_ServiceKey, ServiceDefinition] = {}
        self._workers: dict[_WorkerKey, WorkerDefinition] = {}
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

    def register_telemetry(
        self,
        kinds: Iterable[object],
        scope: object,
        queue: int,
        overflow: object,
        batch: int | None,
        replay: bool,
        replay_limit: int,
        handler: object,
    ) -> object:
        """Records one static telemetry subscription during import."""

        key = f"{getattr(handler, '__module__', '')}.{getattr(handler, '__qualname__', '')}"
        definition = TelemetryDefinition(
            tuple(str(kind) for kind in kinds),
            str(scope),
            queue,
            str(overflow),
            batch,
            replay,
            replay_limit,
            handler,
        )
        self._insert(self._telemetry, key, definition, "telemetry")
        return handler

    def register_prompt_slot(
        self, slot: str, priority: int, cls: object, renderer: object
    ) -> object:
        """Records one static prompt-slot contribution during import."""

        callable_key = (
            f"{getattr(renderer, '__module__', '')}."
            f"{getattr(renderer, '__qualname__', '')}"
        )
        key = (slot, callable_key)
        definition = PromptSlotDefinition(slot, priority, str(cls), renderer)
        self._insert(self._prompt_slots, key, definition, "prompt slot")
        return renderer

    def register_entry_kind(
        self,
        name: str,
        rev: str,
        display: bool | None,
        spill: bool,
        implementation: type,
    ) -> type:
        """Records one typed journal entry declaration during import."""

        key = _entry_kind_key(name, rev)
        if not isinstance(implementation, type):
            raise TypeError("@omp.entry_kind may decorate only a class")
        if display is not None and not isinstance(display, bool):
            raise TypeError("entry kind display must be bool or None")
        if not isinstance(spill, bool):
            raise TypeError("entry kind spill must be bool")
        definition = EntryKindDefinition(
            key[0], key[1], display, spill, implementation
        )
        self._insert(self._entry_kinds, key, definition, "entry kind")
        return implementation


    def entry_kind_definitions(self) -> tuple[EntryKindDefinition, ...]:
        """Returns entry-kind rows in deterministic declaration-key order."""

        return tuple(self._entry_kinds[key] for key in sorted(self._entry_kinds))
    def register_provider(
        self, provider_id: str, spec: object, implementation: type
    ) -> type:
        """Record one pure provider catalog declaration during import."""

        if not isinstance(provider_id, str) or not provider_id:
            raise ValueError("provider id must be a non-empty string")
        if not isinstance(implementation, type):
            raise TypeError("@omp.provider may decorate only a class")
        definition = ProviderDefinition(provider_id, spec, implementation)
        self._insert(self._providers, provider_id, definition, "provider")
        return implementation

    def provider_definitions(self) -> tuple[ProviderDefinition, ...]:
        """Return provider declarations in deterministic identifier order."""

        return tuple(self._providers[key] for key in sorted(self._providers))

    def register_worker(self, name: str, spec: object) -> None:
        """Record one worker manifest projection during import."""

        if not isinstance(name, str) or not name:
            raise ValueError("worker name must be a non-empty string")
        self._insert(self._workers, name, WorkerDefinition(name, spec), "worker")

    def worker_definitions(self) -> tuple[WorkerDefinition, ...]:
        """Return worker declarations in deterministic name order."""

        return tuple(self._workers[key] for key in sorted(self._workers))




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
            entry_kinds=self.entry_kind_definitions(),
            tools=frozenset(self._tools),
            hooks=frozenset(self._hooks),
            services=frozenset(self._services),
            telemetry=tuple(self._telemetry[key] for key in sorted(self._telemetry)),
            prompt_slots=tuple(
                self._prompt_slots[key] for key in sorted(self._prompt_slots)
            ),
            providers=self.provider_definitions(),
            workers=self.worker_definitions(),
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
        if (
            len(self._tools)
            + len(self._hooks)
            + len(self._services)
            + len(self._entry_kinds)
            + len(self._telemetry)
            + len(self._prompt_slots)
            + len(self._providers)
            + len(self._workers)
            >= MAX_DECLARATIONS
        ):
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


def entry_kind(
    name: str,
    *,
    rev: str,
    display: bool | None = None,
    spill: bool = True,
) -> Callable[[_T], _T]:
    """Declare a typed, versioned session-journal entry kind."""

    key = _entry_kind_key(name, rev)

    def decorate(implementation: _T) -> _T:
        registry.register_entry_kind(
            key[0], key[1], display, spill, implementation
        )
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


def _entry_kind_key(name: str, rev: str) -> _EntryKindKey:
    if not isinstance(name, str) or "." not in name or name.startswith("omp."):
        raise ValueError("entry kind name must be a non-core globally qualified name")
    if not isinstance(rev, str):
        raise TypeError("entry kind rev must be a string")
    family, separator, number = rev.rpartition(".")
    if not separator or not family or not number.isascii() or not number.isdigit():
        raise ValueError("entry kind rev must have the form '<family>.<n>'")
    return name, rev


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
    "EntryKindDefinition",
    "QuotaStatus",
    "ResourceReceipt",
    "ServiceClient",
    "ServiceDefinition",
    "Services",
    "entry_kind",
    "configure_manifest",
    "dispatch_service",
    "freeze_declarations",
    "registry",
    "resources",
    "service",
    "services",
)
