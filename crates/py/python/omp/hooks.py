"""Pure-Python hook declarations and decisions.

Importing this module only records declarations.  The CONTROL dispatcher is a
separate host arm and is deliberately unavailable in the surface-freeze wave.
"""

from __future__ import annotations

from collections.abc import Callable, Mapping
from dataclasses import dataclass
from enum import StrEnum
from typing import Any, ClassVar, Final, TypeAlias, TypeVar

from _omp import Duration, OmpError

from ._errors import NotWiredError
from ._registry import registry


_HookFn = TypeVar("_HookFn", bound=Callable[..., object])


class UnknownEvent(ValueError):
    """A hook declaration or catalog lookup named no frozen event."""


class HookContractError(ValueError):
    """A hook declaration or decision violates the frozen hook contract."""


class LateRegistration(RuntimeError):
    """A hook was declared after the extension declaration table was sealed."""


class ReentrancyError(OmpError):
    """A hook exceeded ``omp.limits.REENTRANCY_DEPTH``."""


class PhaseConflict(OmpError):
    """A hook awaited a CONTROL operation blocked by its pending loop phase."""


class HookPhase(StrEnum):
    """Order one hook within the per-event decision procedure."""

    PRECHECK = "precheck"
    TRANSFORM = "transform"
    REVIEW = "review"
    APPROVAL = "approval"
    OBSERVE = "observe"


class CallOrigin(StrEnum):
    """Identify who issued a logical call."""

    MODEL = "model"
    USER = "user"
    SUBAGENT = "subagent"
    REPLAY = "replay"


class TargetKind(StrEnum):
    """Discriminate built-in, extension-device, and MCP dispatch targets."""

    CORE = "core"
    DEVICE = "device"
    MCP = "mcp"




class OnFailure(StrEnum):
    """Select fail-open or fail-closed behavior for an unavailable handler."""

    DEFER = "defer"
    DENY = "deny"


class ApprovalKind(StrEnum):
    """Classify an approval for presentation and configuration lookup."""

    EXEC = "exec"
    WRITE = "write"
    READ = "read"
    NETWORK = "network"
    PRIVILEGE = "privilege"
    DEVICE = "device"
    SPAWN = "spawn"


class PolicyScope(StrEnum):
    """Bound the lifetime of a policy decision or approval grant."""

    ONCE = "once"
    CALL = "call"
    TURN = "turn"
    SESSION = "session"
    PERSIST = "persist"


class ApprovalRoute(StrEnum):
    """Choose where Core routes a durable approval ticket."""

    AUTO = "auto"
    LOCAL = "local"
    PARENT = "parent"
    EXTERNAL = "external"
    NONE = "none"


class Unreachable(StrEnum):
    """Resolve an approval whose selected route cannot answer."""

    FAIL_CLOSED = "fail_closed"
    ESCALATE_LOCAL = "escalate_local"
    FAIL_OPEN_AUDITED = "fail_open_audited"


APPROVAL_DEADLINE: Final[Duration] = Duration("5m")
"""Default wall-clock deadline carried by a durable approval request."""


@dataclass(frozen=True, slots=True)
class ApprovalSpec:
    """Describe one reason to open or merge into a durable approval ticket."""

    title: str
    body: str
    subject: str
    kind: ApprovalKind = ApprovalKind.EXEC
    scopes: tuple[PolicyScope, ...] = (PolicyScope.ONCE, PolicyScope.SESSION)
    default: bool | None = None
    route: ApprovalRoute = ApprovalRoute.AUTO
    approver: str | None = None
    timeout: Duration = APPROVAL_DEADLINE
    unreachable: Unreachable = Unreachable.FAIL_CLOSED
    require_human: bool = False
    pattern: str | None = None
    evidence: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class Allow:
    """Cast an affirmative hook vote without bypassing later phases."""

    reason: str | None = None


@dataclass(frozen=True, slots=True)
class Deny:
    """Refuse an event and optionally classify the refusal durably."""

    reason: str
    fatal: bool = False
    code: str | None = None


@dataclass(frozen=True, slots=True)
class Modify:
    """Replace or shallow-patch the mutable fields of a hook payload."""

    target: CallTarget | None = None
    args: Mapping[str, Any] | None = None
    patch: Mapping[str, Any] | None = None
    reason: str | None = None

    def __post_init__(self) -> None:
        if self.args is not None and self.patch is not None:
            raise HookContractError("Modify args and patch are mutually exclusive")


@dataclass(frozen=True, slots=True)
class Defer:
    """Abstain from a hook decision while optionally recording a debug note."""

    note: str | None = None


@dataclass(frozen=True, slots=True)
class RequireApproval:
    """Complete a hook by asking Core to file a durable approval ticket."""

    spec: ApprovalSpec


HookDecision: TypeAlias = Allow | Deny | Modify | Defer | RequireApproval
"""The closed five-arm return vocabulary for gateable hooks."""


UNSET: Final[object] = object()
"""Sentinel used by ``Modify.patch`` to remove a mapping key."""


@dataclass(frozen=True, slots=True)
class CoreTool:
    """Identify one built-in harness tool dispatch."""

    kind: ClassVar[TargetKind] = TargetKind.CORE
    name: str
    rev: str
    args: Mapping[str, Any]


@dataclass(frozen=True, slots=True)
class DeviceCall:
    """Identify one extension or mounted-device dispatch."""

    kind: ClassVar[TargetKind] = TargetKind.DEVICE
    name: str
    family: str
    rev: str
    args: Mapping[str, Any]


@dataclass(frozen=True, slots=True)
class McpCall:
    """Identify one tool on a mounted MCP server."""

    kind: ClassVar[TargetKind] = TargetKind.MCP
    server: str
    tool: str
    args: Mapping[str, Any]


CallTarget: TypeAlias = CoreTool | DeviceCall | McpCall
"""Discriminated target of a logical tool dispatch."""


@dataclass(frozen=True, slots=True)
class When:
    """Declare a Core-side pre-filter evaluated before payload construction."""

    target: frozenset[TargetKind] | None = None
    name: frozenset[str] | None = None
    server: frozenset[str] | None = None
    rev: frozenset[str] | None = None
    path_globs: tuple[str, ...] = ()
    origin: frozenset["CallOrigin"] | None = None
    reason: frozenset[str] | None = None
    provider: frozenset[str] | None = None
    once: bool = False
    after_gap: Duration | None = None

    def __post_init__(self) -> None:


        for field in ("target", "name", "server", "rev", "origin", "reason", "provider"):
            value = getattr(self, field)
            if value is not None and not isinstance(value, frozenset):
                object.__setattr__(self, field, frozenset(value))
        if not isinstance(self.path_globs, tuple):
            object.__setattr__(self, "path_globs", tuple(self.path_globs))


_EVENT_NAMES = (
    "session_start", "session_shutdown", "session_switch", "session_switched",
    "session_branch", "session_branched", "session_rewind", "session_rewound",
    "session_reset", "before_agent_start", "agent_start", "turn_start", "turn_end",
    "agent_settled", "agent_end", "interrupt", "deadline", "message_start",
    "message_update", "message_end", "item_committed", "call_open", "tool_call",
    "tool_execution_start", "tool_update", "tool_execution_end", "tool_result",
    "tool_approval_requested", "tool_approval_resolved", "device_list", "user_input",
    "user_bash", "user_eval", "command_invoke", "resources_discover",
    "resources_changed", "provider_login", "provider_refresh", "provider_sign",
    "before_request", "models_discover", "provider_error", "provider_usage", "search_parse",
    "sandbox_profile", "sandbox_violation",
    "capability_budget", "model_changed", "credential_disabled", "compaction",
    "compaction_done", "context_reset", "thread_projection", "subagent_spawn", "worker_state",
    "job_registered", "job_settled", "extension_activate", "extension_load",
    "extension_unload", "host_reconnect",
)


@dataclass(frozen=True, slots=True)
class _HookDeclaration:
    event: str
    phase: HookPhase | str
    handler: Callable[..., object]
    order: int
    on_failure: OnFailure | None
    timeout: Duration | None
    coalesce: Duration | None
    when: When | None
    concurrency: int
    threadsafe: bool
    name: str


_OBSERVATION_EVENTS = frozenset(
    {
        "session_shutdown", "session_switched", "session_branched", "session_rewound",
        "session_reset", "agent_start", "turn_end", "agent_end", "interrupt", "deadline",
        "message_start", "message_update", "message_end", "item_committed", "call_open",
        "tool_execution_start", "tool_update", "tool_execution_end",
        "tool_approval_requested", "tool_approval_resolved", "resources_changed",
        "capability_budget", "model_changed", "credential_disabled",
        "compaction_done", "context_reset", "worker_state", "job_registered", "job_settled",
        "extension_activate", "extension_load", "extension_unload", "host_reconnect",
    }
)
_DOMAIN_EVENTS = frozenset(
    {
        "agent_settled",
        "compaction",
        "models_discover",
        "provider_refresh",
        "provider_error",
        "provider_usage",
        "search_parse",
        "sandbox_violation",
        "thread_projection",
    }
)
_STREAM_EVENTS = frozenset({"message_start", "message_update", "message_end", "call_open", "tool_update"})


def hook(
    event: str,
    *,
    phase: HookPhase | None = None,
    order: int = 0,
    on_failure: OnFailure | None = None,
    timeout: Duration | None = None,
    coalesce: Duration | None = None,
    when: When | None = None,
    provider: str | None = None,
    concurrency: int = 1,
    threadsafe: bool = False,
    name: str | None = None,
) -> Callable[[_HookFn], _HookFn]:
    """Declare one hook subscription without performing host I/O."""

    if event not in _EVENT_NAMES:
        raise UnknownEvent(f"unknown hook event {event!r}")
    if registry.sealed:
        raise LateRegistration("hook declarations are sealed")
    if isinstance(phase, str):
        try:
            phase = HookPhase(phase)
        except ValueError as error:
            raise HookContractError(f"unknown hook phase {phase!r}") from error
    if phase is not None and not isinstance(phase, HookPhase):
        raise TypeError("phase must be HookPhase or None")
    if event in _DOMAIN_EVENTS:
        if phase is not None:
            raise HookContractError(f"domain event {event!r} does not accept phase")
        registry_phase: HookPhase | str = "domain"
    elif event in _OBSERVATION_EVENTS:
        if phase not in (None, HookPhase.OBSERVE):
            raise HookContractError(f"observation event {event!r} only accepts OBSERVE")
        registry_phase = HookPhase.OBSERVE
    elif event == "sandbox_profile":
        if phase != HookPhase.TRANSFORM:
            raise HookContractError("sandbox_profile requires TRANSFORM phase")
        registry_phase = phase
    else:
        if phase is None:
            raise HookContractError(f"gateable event {event!r} requires phase")
        registry_phase = phase
    if isinstance(on_failure, str):
        try:
            on_failure = OnFailure(on_failure)
        except ValueError as error:
            raise HookContractError(f"unknown hook failure policy {on_failure!r}") from error
    if event in _OBSERVATION_EVENTS and on_failure is not None:
        raise HookContractError("observation hooks do not accept on_failure")
    if not isinstance(order, int) or isinstance(order, bool):
        raise TypeError("order must be an integer")
    if registry_phase != HookPhase.TRANSFORM and order != 0:
        raise HookContractError("order is legal only in TRANSFORM")
    if event in _STREAM_EVENTS and coalesce is None:
        raise HookContractError(f"stream event {event!r} requires coalesce")
    if event not in _STREAM_EVENTS and coalesce is not None:
        raise HookContractError(f"non-stream event {event!r} does not accept coalesce")
    if provider is not None:
        if when is not None and when.provider is not None:
            raise HookContractError("provider and When.provider are mutually exclusive")
        when = When(provider=frozenset({provider})) if when is None else When(
            target=when.target, name=when.name, server=when.server, rev=when.rev,
            path_globs=when.path_globs, origin=when.origin, reason=when.reason,
            provider=frozenset({provider}), once=when.once, after_gap=when.after_gap,
        )
    if isinstance(concurrency, bool) or not isinstance(concurrency, int) or concurrency < 1:
        raise ValueError("concurrency must be a positive integer")
    if not isinstance(threadsafe, bool):
        raise TypeError("threadsafe must be bool")

    def decorate(handler: _HookFn) -> _HookFn:
        if not callable(handler):
            raise TypeError("@omp.hook may decorate only a callable")
        stable_name = name or f"{handler.__module__}.{handler.__qualname__}"
        if not stable_name:
            raise ValueError("hook name must be non-empty")
        declaration = _HookDeclaration(
            event, registry_phase, handler, order, on_failure, timeout, coalesce,
            when, concurrency, threadsafe, stable_name,
        )
        registry.register_hook(event, registry_phase, declaration)
        prior = tuple(getattr(handler, "__omp_hooks__", ()))
        setattr(handler, "__omp_hooks__", prior + (declaration,))
        return handler

    return decorate


async def dispatch_hook(*_args: object, **_kwargs: object) -> object:
    """Fail at call time until Part 4 installs the CONTROL hook dispatcher."""

    raise NotWiredError("omp hook CONTROL dispatch is not wired")

__all__ = (
    "APPROVAL_DEADLINE", "Allow", "ApprovalKind", "ApprovalRoute", "ApprovalSpec",
    "CallOrigin", "CallTarget", "CoreTool", "Defer", "Deny", "DeviceCall", "HookContractError",
    "HookDecision", "HookPhase", "LateRegistration", "McpCall", "Modify", "OnFailure",
    "PhaseConflict", "PolicyScope", "ReentrancyError", "RequireApproval", "TargetKind",
    "UNSET", "UnknownEvent", "Unreachable",
    "When", "dispatch_hook", "hook",
)
