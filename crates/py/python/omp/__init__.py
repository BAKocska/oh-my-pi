"""The frozen omp Python extension API.

The package is declarative at import time: importing it performs no filesystem,
network, subprocess, Environment, CONTROL, or DATA operation.
"""

from __future__ import annotations

import asyncio as _asyncio
import contextvars as _contextvars
import inspect as _inspect
import os as _os
import re as _re
from dataclasses import KW_ONLY as _KW_ONLY
from dataclasses import dataclass as _dataclass
from enum import StrEnum as _StrEnum
from collections.abc import Callable as _Callable
from collections.abc import Mapping as _Mapping
from collections.abc import Sequence as _Sequence
from types import MappingProxyType as _MappingProxyType
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
    Secret,
    StaleGeneration,
    TrustError,
    WorkspaceUri,
    _scheme_snapshot,
    _phase_legality_matrix,
    _runtime_metadata,
    operation_spec as _native_operation_spec,
)


class Coerce(_StrEnum):
    """Name one declared, journaled argument coercion."""

    LOOSE_BOOL = "loose_bool"
    INTEGER = "integer"
    NUMBER = "number"
    STRING = "string"
    SINGLETON = "singleton"
    JSON_STRING = "json_string"
    STRIP = "strip"
    CSV = "csv"
    NULL_ELISION = "null_elision"


@_dataclass(frozen=True, slots=True)
class Field:
    """Carry declarative metadata for one ``Annotated`` device argument."""

    description: str | None = None
    _: _KW_ONLY
    additional_properties: bool = False
    alias: tuple[str, ...] = ()
    coerce: tuple[Coerce, ...] = ()
    expected: str | None = None
    example: str | None = None

    def __post_init__(self) -> None:
        """Validate and freeze aliases and coercions at declaration time."""

        if self.description is not None and not isinstance(self.description, str):
            raise TypeError("field description must be str or None")
        if not isinstance(self.additional_properties, bool):
            raise TypeError("field additional_properties must be bool")
        if isinstance(self.alias, str):
            raise TypeError("field alias must be a tuple of strings")
        aliases = tuple(self.alias)
        if any(not isinstance(alias, str) or not alias for alias in aliases):
            raise TypeError("field aliases must be non-empty strings")
        if len(set(aliases)) != len(aliases):
            raise ValueError("field aliases must be unique")
        coercions = tuple(self.coerce)
        if any(not isinstance(coercion, Coerce) for coercion in coercions):
            raise TypeError("field coercions must contain only Coerce members")
        if self.expected is not None and not isinstance(self.expected, str):
            raise TypeError("field expected must be str or None")
        if self.example is not None and not isinstance(self.example, str):
            raise TypeError("field example must be str or None")
        object.__setattr__(self, "alias", aliases)
        object.__setattr__(self, "coerce", coercions)


class Fault:
    """Marker base for a device's durable typed failure value."""

    __slots__ = ()

    def __new__(cls, *_args: _Any, **_kwargs: _Any) -> Fault:
        if cls is Fault:
            raise TypeError("Fault is a marker base; instantiate a frozen dataclass subclass")
        return super().__new__(cls)

    def useless(self) -> bool:
        """Return whether compaction may omit this value's prompt projection."""
        return False




from ._context import Context
from ._errors import ExtensionError, NotWiredError
from ._scope import Trust
from ._verdicts import (
    ArtifactLifetime,
    BlobPart,
    Budget,
    Dialect,
    Done,
    Faulted,
    JsonPart,
    LiftedCall,
    ModelClass,
    Ok,
    Part,
    Payload,
    PromptCaps,
    RecordedCall,
    Rev,
    SPILL_INLINE_LIMIT,
    SpillBudget,
    TextPart,
    ToolIdentity,
    Update,
    View,
    prompt,
)

class StateScopeDenied(OmpError):
    """The authenticated principal may not access a requested state scope."""


class PermissionDenied(PermissionError, OmpError):
    """The authenticated principal lacks permission for a requested operation."""

_control_backend: _contextvars.ContextVar[_Any | None] = _contextvars.ContextVar(
    "omp_control_backend", default=None
)


def _install_control_backend(backend: _Any) -> None:
    """Install the host-owned CONTROL bridge in the active invocation context."""
    _control_backend.set(backend)
    from . import _context as _context_module
    from . import telemetry as _telemetry
    from . import ui as _ui

    _ui._install_effect_sink(getattr(backend, "effect", None))
    _telemetry._install_instrument_sink(getattr(backend, "instrument", None))
    _context_module._install_log_sink(getattr(backend, "log", None))


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


class Capability(_StrEnum):
    """Manifest capabilities required by extension declarations."""

    PLACE_ENV = "place.env"
    PLACE_WORKER = "place.worker"
    SCHEDULES_PROJECT = "schedules:project"

# Importing these frozen modules only creates declarations and namespace values.
from . import env as env
from . import urls as urls
from . import journal as journal
from .journal import EntryId, JournalEntry
from . import ui as ui
from . import agents as agents
from . import prompts as prompts
from . import sessions as sessions
from . import telemetry as telemetry
from . import context as context
from . import policy as policy
from . import limits as limits
from . import creds as creds
from . import secrets as secrets
from .creds import CredentialMeta, ScopedToken
from .secrets import SecretKind, SecretMode, SecretRule
from .prompts import (
    PromptContext,
    SlotClass,
    SlotClassConflict,
    UnknownSlot,
    prompt_slot,
)
from .context import (
    Anchor,
    CancelCompaction,
    CompactionBusy,
    CompactionEvent,
    CompactionOutcome,
    CompactionRefused,
    CompactionTier,
    CompactionVerdict,
    ContextGone,
    ContextPatch,
    ContextResetEvent,
    ContextUsage,
    ContextView,
    CustomSummary,
    DelegateCompaction,
    Insert,
    MessageKind,
    MessageRef,
    NoVerdict,
    PatchRejected,
    PinBudgetExceeded,
    Prune,
    Reorder,
    Replace,
    StaleEpoch,
    ToolRef,
)
from .sessions import (
    Bucket,
    GroupBy,
    SessionFilter,
    SessionInfo,
    SessionKind,
    SessionLink,
    SessionNotFound,
    SessionStatus,
    TitleSource,
    Usage,
    UsageAccuracy,
    UsageBucket,
    UsageQuery,
    UsageReport,
)
from .telemetry import PromptFingerprint
renderer = ui.renderer
command = ui.command
shortcut = ui.shortcut
DuplicateRenderer = ui.DuplicateRenderer
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
    DeviceDefinition as _DeviceDefinition,
    DeclarationRegistry,
    MAX_DECLARATIONS,
    DeclarationSnapshot,
    QuotaStatus,
    ResourceReceipt,
    ServiceClient,
    ServiceDefinition,
    Services,
    resources,
    entry_kind,
    service,
    services,
    registry as _declarations,
)
from .placement import (
    BoundaryError,
    MAX_WORKERS,
    Place,
    PlaceKind,
    Restart,
    ShipError,
    Site,
    SiteKind,
    Spill,
    WorkerHandle,
    WorkerInfo,
    WorkerResources,
    WorkerSpec,
    WorkerState,
    WorkerUnavailable,
    worker_state,
    workers,
)
from .policy import (
    APPROVAL_DEADLINE,
    Access,
    Amend,
    AndOrOp,
    approver,
    ApprovalDecision,
    ApprovalSource,
    ApprovalTicket,
    BASH_IR_MAX_DEPTH,
    BASH_IR_MAX_NODES,
    BASH_IR_MAX_SOURCE,
    BASH_IR_REV,
    BashAndOrList,
    BashArg,
    BashAssignment,
    BashCommandIR,
    BashCompound,
    BashFunctionDef,
    BashIR,
    BashNode,
    BashPipeline,
    BashRedirect,
    BashTestExpr,
    CompoundKind,
    DnsPolicy,
    DomainRule,
    Dynamism,
    EnforcementUnavailable,
    ExecPolicy,
    FilesystemGrade,
    FilesystemPolicy,
    HereDoc,
    NetDirection,
    NetKind,
    NetRef,
    NetworkGrade,
    NetworkMode,
    NetworkPolicy,
    OpaqueEvaluator,
    OpaqueReason,
    POLICY_DEADLINE,
    ParseError,
    ParseFailure,
    PathOrigin,
    PathRef,
    PathRule,
    PolicyDenied,
    PolicyError,
    ProcessGrade,
    ProcessSubDirection,
    ProcessSubIR,
    ProfileHandle,
    ProfileRejected,
    ProfileWidened,
    Quoting,
    RedirectOp,
    RedirectTarget,
    ResourceBudget,
    RuleEffect,
    RuleRef,
    SandboxBackend,
    SandboxCapabilities,
    SandboxEnforcement,
    SandboxMode,
    SandboxProfile,
    SandboxRequest,
    Separator,
    SandboxSessionKind,
    Span,
    TicketState,
    Tier,
    VIOLATION_COALESCE,
    Violation,
    ViolationKind,
)
from .devices import (
    Availability,
    AvailabilityDelta,
    Devices,
    Device,
    DeviceError,
    DeviceInfo,
    DeviceNameError,
    DeviceUnavailable,
    DocEffects,
    DocsBudgetError,
    DocsMode,
    DynamicDeviceParent,
    EXTERNAL_SUMMARY_CAP,
    Effects,
    Example,
    ExecEffects,
    InferenceEffects,
    MountSpec,
    PER_DEVICE_CAP,
    Precedence,
    PrecedenceConflict,
    SchemaError,
    ToolPath,
    devices,
)
from .provider import (
    AccountScope,
    Api,
    AuthMode,
    AuthSpec,
    CacheRetention,
    Cap,
    CatalogAlias,
    ChatCaps,
    CodecProfile,
    CompatFlags,
    Completion,
    ContextSpec,
    Cost,
    CostTier,
    Credential,
    CredentialKind,
    CredentialSource,
    Dimensions,
    DiscoveryDefaults,
    DiscoveryKind,
    DiscoveryPage,
    DiscoveryQuery,
    DiscoverySpec,
    ErrorKind,
    Failover,
    FailoverKind,
    Fallback,
    Effort,
    ImageCaps,
    ImageFeature,
    ImageFormat,
    ImageRequest,
    ImageResult,
    LoginRequest,
    LogprobCaps,
    ManagementSpec,
    Modality,
    ModelOverlay,
    ModelPatch,
    ModelSpec,
    OAuthFlow,
    OAuthFlowKind,
    Intent,
    IntentKind,
    ModelFallback,
    ModelRef,
    OAuthSpec,
    Pagination,
    Operation,
    PrincipalResolution,
    PromptCacheCaps,
    ProviderSpec,
    ProviderHandle,
    ReasoningCaps,
    RefreshBehavior,
    RefreshReason,
    RefreshRequest,
    ProviderError,
    Retryability,
    RedirectTrust,
    RouteLimits,
    RouteRef,
    RouteSpec,
    ScopedAlias,
    ServerStateCaps,
    ServiceTier,
    SignRequest,
    ThinkingMode,
    ThinkingSpec,
    TokenPlacement,
    ToolCaps,
    ToolFeature,
    ToolSchemaFlavor,
    Transport,
    TrustDomain,
    provider,
)
from . import hooks as hooks
from .hooks import *
from . import events as events
from .events import *
# Hooks and policy document the same top-level approval deadline; policy owns
# the assembled policy vocabulary.
APPROVAL_DEADLINE = policy.APPROVAL_DEADLINE


from . import index as index
from . import packages as packages
from .diagnostics import DiagnosticCode, FailureCode, WarningCode
from .packages import (
    Distribution,
    GrantError,
    IntegrityError,
    Origin,
    PackageError,
    Provenance,
    ResolutionError,
    SettingSchema,
    SiteTree,
)


_DEVICE_NAME_PATTERN = _re.compile(r"^[a-z][a-z0-9_]{0,63}$")
_RESERVED_DEVICE_NAMES = frozenset(
    {"resolve", "reject", "propose", "report_issue"}
)


def device(
    name: str | None = None,
    *,
    family: str = "",
    rev: int = 1,
    place: str | Place = "host",
    summary: str | None = None,
    docs: str | _os.PathLike[str] | None = None,
    schema: type | dict[str, object] | None = None,
    examples: _Sequence[Example] = (),
    available: _Callable[[], bool | Availability] | None = None,
    precedence: int = Precedence.DEFAULT,
    replaces: str | None = None,
    intents: _Sequence[Intent] = (),
    effects: Effects | None = None,
    tier: Tier = Tier.WRITE,
    deadline: Duration | None = None,
    aliases: _Mapping[str, str] | None = None,
) -> _Callable[[_Any], Device]:
    """Declare a device while deferring its availability predicate to FREEZE."""
    parsed_place = Place.parse(place)
    if not isinstance(rev, int) or isinstance(rev, bool):
        raise TypeError("device rev must be int")
    if not isinstance(precedence, int) or isinstance(precedence, bool):
        raise TypeError("device precedence must be int")
    if precedence >= Precedence.CORE:
        raise DeviceNameError(
            f"device precedence must be below Precedence.CORE: {precedence}"
        )
    if schema is not None and not isinstance(schema, (type, dict)):
        raise SchemaError("device schema must be a type, dict, or None")
    if available is not None and not callable(available):
        raise SchemaError("device available predicate must be callable")

    frozen_examples = tuple(examples)
    if any(not isinstance(example, Example) for example in frozen_examples):
        raise SchemaError("device examples must contain only Example values")

    frozen_aliases: _Mapping[str, str] | None = None
    if aliases is not None:
        if not isinstance(aliases, _Mapping):
            raise SchemaError("device aliases must be a mapping")
        seen_aliases: set[str] = set()
        alias_items: list[tuple[str, str]] = []
        for alias, target in aliases.items():
            if not isinstance(alias, str) or not isinstance(target, str):
                raise SchemaError("device aliases must map strings to strings")
            if alias in seen_aliases:
                raise SchemaError(f"duplicate device alias {alias!r}")
            if alias == target:
                raise SchemaError(f"device alias {alias!r} cannot map to itself")
            seen_aliases.add(alias)
            alias_items.append((alias, target))
        frozen_aliases = _MappingProxyType(dict(alias_items))

    frozen_intents = tuple(intents)

    def decorate(body: _Any) -> Device:
        if not callable(body):
            raise TypeError("@omp.device may decorate only a callable")
        resolved_name = (
            getattr(body, "__name__", "").lstrip("_") if name is None else name
        )
        if (
            not isinstance(resolved_name, str)
            or _DEVICE_NAME_PATTERN.fullmatch(resolved_name) is None
        ):
            raise DeviceNameError(f"invalid device name {resolved_name!r}")
        if resolved_name in _RESERVED_DEVICE_NAMES:
            raise DeviceNameError(f"reserved device name {resolved_name!r}")


        handle = Device(
            name=resolved_name,
            family=family,
            rev=rev,
            place=parsed_place,
            precedence=precedence,
            replaces=replaces,
            schema=schema,
            docs=docs,
            summary=summary,
            body=body,
        )
        definition = _DeviceDefinition(
            name=resolved_name,
            family=family,
            rev=rev,
            place=parsed_place,
            summary=summary,
            docs=docs,
            schema=schema,
            examples=frozen_examples,
            available=available,
            precedence=precedence,
            replaces=replaces,
            intents=frozen_intents,
            effects=effects,
            tier=tier,
            deadline=deadline,
            aliases=frozen_aliases,
            body=body,
        )
        try:
            body.__omp_place__ = parsed_place
        except (AttributeError, TypeError):
            pass
        _declarations.register_tool(
            resolved_name,
            family,
            rev,
            handle,
            definition=definition,
        )
        return handle

    return decorate


def tool(
    name: str | _Any | None = None,
    *,
    kind: str = "soft",
    effects: _Any = None,
    tier: _Any = None,
    rev: int = 1,
):
    """Declare an ergonomic host leaf on the existing device registry path."""
    if kind not in {"soft", "hard"}:
        raise ValueError("tool kind must be 'soft' or 'hard'")
    if not isinstance(rev, int) or isinstance(rev, bool):
        raise TypeError("tool rev must be int")

    def decorate(function: _Any) -> _Any:
        tool_name = function.__name__ if name is None or callable(name) else name
        function.__omp_place__ = Place.HOST
        function.__omp_tool_kind__ = kind
        function.__omp_effects__ = effects
        function.__omp_tier__ = tier
        definition = _DeviceDefinition(
            name=tool_name,
            family="",
            rev=rev,
            place=Place.HOST,
            summary=None,
            docs=None,
            schema=None,
            examples=(),
            available=None,
            precedence=Precedence.DEFAULT,
            replaces=None,
            intents=(),
            effects=effects,
            tier=tier,
            deadline=None,
            aliases=None,
            body=function,
        )
        _declarations.register_tool(
            tool_name, "", rev, function, definition=definition
        )
        return function

    if callable(name):
        return decorate(name)
    return decorate
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
    "Coerce",
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
    "ExtensionError",
    "EnvUnavailable",
    "EntryId",
    "ArtifactLifetime",
    "BlobPart",
    "Budget",
    "Context",
    "Field",
    "Fault",
    "Bucket",
    "AccountScope",
    "Api",
    "AuthMode",
    "AuthSpec",
    "Availability",
    "AvailabilityDelta",
    "CacheRetention",
    "Cap",
    "CatalogAlias",
    "ChatCaps",
    "CodecProfile",
    "CompatFlags",
    "Completion",
    "ContextSpec",
    "Cost",
    "CostTier",
    "Credential",
    "CredentialKind",
    "CredentialMeta",
    "CredentialSource",
    "Dimensions",
    "DiscoveryDefaults",
    "DiscoveryKind",
    "DiscoveryPage",
    "DiscoveryQuery",
    "DiscoverySpec",
    "Device",
    "DeviceError",
    "DeviceInfo",
    "DeviceNameError",
    "DeviceUnavailable",
    "DocEffects",
    "DocsBudgetError",
    "DocsMode",
    "DynamicDeviceParent",
    "Effects",
    "Example",
    "ExecEffects",
    "Effort",
    "ImageCaps",
    "ImageFeature",
    "ImageFormat",
    "ImageRequest",
    "ImageResult",
    "LoginRequest",
    "LogprobCaps",
    "InferenceEffects",
    "ManagementSpec",
    "Modality",
    "ModelOverlay",
    "ModelPatch",
    "ModelSpec",
    "MountSpec",
    "OAuthFlow",
    "OAuthFlowKind",
    "OAuthSpec",
    "Operation",
    "Pagination",
    "PrincipalResolution",
    "Precedence",
    "PrecedenceConflict",
    "PromptCacheCaps",
    "ProviderSpec",
    "ProviderHandle",
    "ReasoningCaps",
    "RefreshBehavior",
    "RefreshReason",
    "RefreshRequest",
    "RedirectTrust",
    "RouteLimits",
    "RouteSpec",
    "ScopedAlias",
    "SchemaError",
    "ServerStateCaps",
    "ServiceTier",
    "SignRequest",
    "ThinkingMode",
    "ThinkingSpec",
    "TokenPlacement",
    "ToolCaps",
    "ToolFeature",
    "ToolSchemaFlavor",
    "ToolPath",
    "Transport",
    "TrustDomain",
    "GroupBy",
    "Dialect",
    "Done",
    "Faulted",
    "FrameTooLarge",
    "HistoryUrl",
    "HostDisconnected",
    "JournalEntry",
    "InvocationPhase",
    "PHASE_LEGALITY_MATRIX",
    "LifecyclePhase",
    "ManifestError",
    "JsonPart",
    "LiftedCall",
    "ModelClass",
    "NotWiredError",
    "PromptContext",
    "PromptFingerprint",
    "Ok",
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
    "Part",
    "Payload",
    "PromptCaps",
    "RecordedCall",
    "Rev",
    "SPILL_INLINE_LIMIT",
    "SpillBudget",
    "TextPart",
    "Update",
    "ToolIdentity",
    "View",
    "StaleGeneration",
    "StateScope",
    "SessionFilter",
    "SessionInfo",
    "SessionKind",
    "SessionLink",
    "SessionNotFound",
    "SessionStatus",
    "SlotClass",
    "SlotClassConflict",
    "TitleSource",
    "UnknownSlot",
    "Usage",
    "UsageAccuracy",
    "UsageBucket",
    "UsageQuery",
    "UsageReport",
    "StateScopeDenied",
    "Url",
    "UrlError",
    "TrustError",
    "WorkspaceUri",
    "env",
    "agents",
    "entry_kind",
    "journal",
    "prompts",
    "prompt_slot",
    "sessions",
    "telemetry",
    "operation_spec",
    "resources",
    "service",
    "parse",
    "parse_selector",
    "services",
    "state",
    "state_dir",
    "urls",
    "ui",
    "schemes",
    "BoundaryError",
    "Capability",
    "MAX_WORKERS",
    "Place",
    "PlaceKind",
    "Restart",
    "ShipError",
    "Site",
    "SiteKind",
    "Spill",
    "WorkerHandle",
    "WorkerInfo",
    "WorkerResources",
    "command",
    "shortcut",
    "DuplicateRenderer",
    "renderer",
    "WorkerSpec",
    "WorkerState",
    "WorkerUnavailable",
    "approver",
    "device",
    "prompt",
    "tool",
    "worker_state",
    "workers",
    "devices",
    "creds",
    "secrets",
    "provider",
    "DiagnosticCode",
    "Distribution",
    "FailureCode",
    "GrantError",
    "IntegrityError",
    "Origin",
    "PackageError",
    "Provenance",
    "ResolutionError",
    "SiteTree",
    "ScopedToken",
    "Secret",
    "SecretKind",
    "SecretMode",
    "SecretRule",
    "WarningCode",
    "index",
    "packages",
)
__all__ += (
    "Access",
    "Amend",
    "Anchor",
    "AndOrOp",
    "ApprovalDecision",
    "ApprovalSource",
    "ApprovalTicket",
    "BASH_IR_MAX_DEPTH",
    "BASH_IR_MAX_NODES",
    "BASH_IR_MAX_SOURCE",
    "BASH_IR_REV",
    "BashAndOrList",
    "BashArg",
    "BashAssignment",
    "BashCommandIR",
    "BashCompound",
    "BashFunctionDef",
    "BashIR",
    "BashNode",
    "BashPipeline",
    "BashRedirect",
    "BashTestExpr",
    "CancelCompaction",
    "CompactionBusy",
    "CompactionEvent",
    "CompactionOutcome",
    "CompactionRefused",
    "CompactionTier",
    "CompactionVerdict",
    "CompoundKind",
    "ContextGone",
    "ContextPatch",
    "ContextResetEvent",
    "ContextUsage",
    "ContextView",
    "CustomSummary",
    "DelegateCompaction",
    "Devices",
    "DnsPolicy",
    "DomainRule",
    "Dynamism",
    "EXTERNAL_SUMMARY_CAP",
    "EnforcementUnavailable",
    "ErrorKind",
    "ExecPolicy",
    "Failover",
    "FailoverKind",
    "Fallback",
    "FilesystemGrade",
    "FilesystemPolicy",
    "HereDoc",
    "Insert",
    "Intent",
    "IntentKind",
    "MessageKind",
    "MessageRef",
    "ModelFallback",
    "ModelRef",
    "NetDirection",
    "NetKind",
    "NetRef",
    "NetworkGrade",
    "NetworkMode",
    "NetworkPolicy",
    "NoVerdict",
    "OpaqueEvaluator",
    "OpaqueReason",
    "PER_DEVICE_CAP",
    "POLICY_DEADLINE",
    "ParseError",
    "ParseFailure",
    "PatchRejected",
    "PathOrigin",
    "PathRef",
    "PathRule",
    "PermissionDenied",
    "PinBudgetExceeded",
    "PolicyDenied",
    "PolicyError",
    "ProcessGrade",
    "ProcessSubDirection",
    "ProcessSubIR",
    "ProfileHandle",
    "ProfileRejected",
    "ProfileWidened",
    "ProviderError",
    "Prune",
    "Quoting",
    "RedirectOp",
    "RedirectTarget",
    "Reorder",
    "Replace",
    "ResourceBudget",
    "Retryability",
    "RouteRef",
    "RuleEffect",
    "RuleRef",
    "SandboxBackend",
    "SandboxCapabilities",
    "SandboxEnforcement",
    "SandboxMode",
    "SandboxProfile",
    "SandboxRequest",
    "SandboxSessionKind",
    "Separator",
    "SettingSchema",
    "Span",
    "StaleEpoch",
    "TicketState",
    "Tier",
    "ToolRef",
    "Trust",
    "VIOLATION_COALESCE",
    "Violation",
    "ViolationKind",
    "context",
    "limits",
    "policy",
)
__all__ += hooks.__all__ + events.__all__ + ("hooks", "events")
