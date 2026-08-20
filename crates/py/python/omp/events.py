"""Frozen hook event payload views.

These payloads are hand-written from ``docs/py/05-hooks.md`` for the Python
surface freeze.  Part 4 replaces their source with generation from the shared
hook protobuf; this module intentionally contains no generator or host I/O.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from datetime import datetime
from enum import StrEnum
from types import MappingProxyType
from typing import Any, Final
from _omp import ActivateReason, ArtifactUrl, BlobRef, Duration, EnvPath


from .agents import Usage
from .hooks import CallOrigin, CallTarget, TargetKind
from .devices import DeviceInfo
from .placement import Place
from .policy import BashIR
from .provider import Effort, Intent, ModelRef, Role, RouteRef
from ._verdicts import ArtifactLifetime
from ._scope import Trust
from .ui import InvocationMode
from .telemetry import StopReason





class InputSource(StrEnum):
    """Identify how a submission entered the harness."""

    INTERACTIVE = "interactive"
    RPC = "rpc"
    EXTENSION = "extension"
    SCHEDULE = "schedule"


class ItemKind(StrEnum):
    """Discriminate durable projected item kinds."""

    MESSAGE = "message"
    TOOL_CALL = "tool_call"
    TOOL_RESULT = "tool_result"
    REASONING = "reasoning"


class ResourceKind(StrEnum):
    """Discriminate extension resource kinds."""

    SKILL = "skill"
    PROMPT = "prompt"
    THEME = "theme"
    RULE = "rule"
    AGENT = "agent"


class OutcomeKind(StrEnum):
    """Discriminate the four durable call outcome arms."""

    OK = "ok"
    FAULTED = "faulted"
    ARGS_REJECTED = "args_rejected"
    ABORTED = "aborted"




class ShutdownReason(StrEnum):
    """Explain why a session is shutting down."""

    USER_EXIT = "user_exit"
    SIGNAL = "signal"
    SWITCH = "switch"
    FATAL = "fatal"
    HOST_REPLACED = "host_replaced"


class SwitchReason(StrEnum):
    """Explain a requested session switch."""

    NEW = "new"
    RESUME = "resume"
    FORK = "fork"
    HANDOFF = "handoff"


class BranchReason(StrEnum):
    """Explain why a session branch was created."""

    USER = "user"
    REWIND = "rewind"
    COMPACTION = "compaction"


class AgentPhase(StrEnum):
    """Mirror the agent loop's coarse execution phase."""

    IDLE = "idle"
    PROJECTING = "projecting"
    TURNING = "turning"
    TOOL_BATCH = "tool_batch"


class TurnInputMode(StrEnum):
    """Describe whether a turn receives a full or delta projection."""

    FULL = "full"
    DELTA = "delta"


class SettleReason(StrEnum):
    """Explain why one caller submission settled."""

    STOP = "stop"
    INTERRUPTED = "interrupted"
    EMPTY_OUTPUT = "empty_output"
    MAILBOX_EMPTY = "mailbox_empty"


class InterruptClass(StrEnum):
    """Select the boundary at which an interrupt drains."""

    IMMEDIATE = "immediate"
    TURN_BOUNDARY = "turn_boundary"
    IDLE = "idle"


class DrainPoint(StrEnum):
    """Name an agent mailbox drain boundary."""

    IMMEDIATE = "immediate"
    TURN_BOUNDARY = "turn_boundary"
    IDLE = "idle"


class InterruptSource(StrEnum):
    """Identify the producer of an interrupt."""

    JOB = "job"
    PRODUCER = "producer"
    USER = "user"
    DEADLINE = "deadline"


class DeadlineScope(StrEnum):
    """Identify which operation exhausted its deadline."""

    AGENT = "agent"
    TURN = "turn"
    CALL = "call"
    HOOK = "hook"


class PartKind(StrEnum):
    """Discriminate a streamed message part delta."""

    TEXT = "text"
    REASONING = "reasoning"
    TOOL_ARGS = "tool_args"
    IMAGE = "image"


class FinishReason(StrEnum):
    """Explain why a streamed message ended."""

    COMPLETE = "complete"
    TRUNCATED = "truncated"
    INTERRUPTED = "interrupted"
    ERROR = "error"


class DeviceListReason(StrEnum):
    """Explain why the effective device list was assembled."""

    SESSION_START = "session_start"
    TOOLSET_CHANGED = "toolset_changed"
    MODE_CHANGED = "mode_changed"
    MODEL_CHANGED = "model_changed"
    MANUAL = "manual"


class EvalLanguage(StrEnum):
    """Name a supported user-evaluation language."""

    PY = "py"
    JS = "js"


class DiscoverReason(StrEnum):
    """Explain why extension resources were rediscovered."""

    STARTUP = "startup"
    RELOAD = "reload"
    WORKSPACE_CHANGED = "workspace_changed"
    EXTENSION_CHANGED = "extension_changed"


class ModelChangeReason(StrEnum):
    """Explain why the selected model changed."""

    USER = "user"
    FALLBACK = "fallback"
    ROLE = "role"
    POLICY = "policy"




class UnloadReason(StrEnum):
    """Explain why an extension host unloaded an extension."""

    USER = "user"
    RELOAD = "reload"
    ERROR = "error"
    QUARANTINE = "quarantine"
    SHUTDOWN = "shutdown"


@dataclass(frozen=True, slots=True)
class CallRef:
    """Reference one call and its resolved target."""

    call_id: str
    target: CallTarget


@dataclass(frozen=True, slots=True)
class ItemRef:
    """Reference one durable projected item."""

    event_index: int
    item_id: str
    kind: ItemKind
    role: Role | None


@dataclass(frozen=True, slots=True)
class SessionOrigin:
    """Identify the source session and optional branch point."""

    session_id: str
    at_event: int | None


@dataclass(frozen=True, slots=True)
class RunSummary:
    """Summarize the durable result of one submission."""

    committed_turns: int
    interrupted: bool
    stop: StopReason | None


@dataclass(frozen=True, slots=True)
class RewindTarget:
    """Describe one item affected by a session rewind."""

    event_index: int
    keep_event: int | None
    text: str


@dataclass(frozen=True, slots=True)
class ResourceRef:
    """Reference one discovered extension resource."""

    uri: EnvPath
    kind: ResourceKind
    origin: str


@dataclass(frozen=True, slots=True)
class Annotation:
    """Attach structured, optionally displayed metadata to an outcome."""

    kind: str
    data: Mapping[str, Any]
    display: bool = True


@dataclass(frozen=True, slots=True)
class SessionStartEvent:
    """Describe a real session-start transition."""

    session_id: str
    root: EnvPath
    cwd: EnvPath
    dirs: tuple[EnvPath, ...]
    resumed: bool
    forked_from: SessionOrigin | None
    agent: str | None
    trust: Trust
    head_event: int
    prompt_rev: str


@dataclass(frozen=True, slots=True)
class SessionShutdownEvent:
    """Describe a session entering its bounded shutdown window."""

    session_id: str
    reason: ShutdownReason
    budget: Duration


@dataclass(frozen=True, slots=True)
class SessionSwitchEvent:
    """Describe a requested session switch before it occurs."""

    reason: SwitchReason
    from_session: str | None
    to_session: str | None
    target_cwd: EnvPath | None


@dataclass(frozen=True, slots=True)
class SessionSwitchedEvent:
    """Describe a completed session switch."""

    reason: SwitchReason
    from_session: str | None
    to_session: str
    head_event: int


@dataclass(frozen=True, slots=True)
class SessionBranchEvent:
    """Describe a requested session branch."""

    at_event: int
    keep_event: int | None
    reason: BranchReason
    summarize: bool


@dataclass(frozen=True, slots=True)
class SessionBranchedEvent:
    """Describe a completed session branch."""

    at_event: int
    new_head: int
    summary_event: int | None


@dataclass(frozen=True, slots=True)
class SessionRewindEvent:
    """Describe a requested session rewind."""

    to_event: int | None
    restore_workspace: bool
    targets: tuple[RewindTarget, ...]
    dropped_items: int


@dataclass(frozen=True, slots=True)
class SessionRewoundEvent:
    """Describe a completed session rewind."""

    to_event: int | None
    new_head: int
    restored_workspace: bool


@dataclass(frozen=True, slots=True)
class SessionResetEvent:
    """Describe a durable session reset marker."""

    at_event: int
    kept_events: int


@dataclass(frozen=True, slots=True)
class BeforeAgentStartEvent:
    """Describe a caller submission before agent execution starts."""

    submission_id: str
    text: str
    items: tuple[ItemRef, ...]
    source: InputSource
    prompt_rev: str
    staged_interrupts: int
    resuming: bool
    schedule_id: str | None = None


@dataclass(frozen=True, slots=True)
class AgentStartEvent:
    """Describe the start of one caller submission."""

    submission_id: str
    from_phase: AgentPhase
    pending_items: int


@dataclass(frozen=True, slots=True)
class TurnStartEvent:
    """Describe a turn after its prompt and toolset are assembled."""

    turn_id: str
    turn_index: int
    prompt_hash: str
    toolset_hash: str
    enabled_tools: tuple[str, ...]
    input_mode: TurnInputMode
    model: ModelRef
    route: RouteRef
    thinking: Effort
    deadline: Duration | None
    attempt: int
    prompt_changed: bool
    toolset_changed: bool


@dataclass(frozen=True, slots=True)
class TurnEndEvent:
    """Describe one committed turn and its settled usage."""

    turn_id: str
    turn_index: int
    event_index: int
    stop: StopReason
    usage: Usage
    session_usage: Usage
    revision: str | None
    calls: tuple[CallRef, ...]
    items: tuple[ItemRef, ...]


@dataclass(frozen=True, slots=True)
class AgentSettledEvent:
    """Describe the single goal-loop seam for a caller submission."""

    submission_id: str
    reason: SettleReason
    committed_turns: int
    last_stop: StopReason | None
    pending_jobs: tuple[str, ...]
    continuations_used: int


@dataclass(frozen=True, slots=True)
class AgentEndEvent:
    """Describe the end of one caller submission."""

    submission_id: str
    summary: RunSummary
    continued: bool
    error: str | None


@dataclass(frozen=True, slots=True)
class InterruptEvent:
    """Describe one interrupt observed at its drain point."""

    source: InterruptSource
    reason: str
    klass: InterruptClass
    drain_point: DrainPoint
    turn_id: str | None


@dataclass(frozen=True, slots=True)
class DeadlineEvent:
    """Describe an exhausted agent, turn, call, or hook deadline."""

    scope: DeadlineScope
    elapsed: Duration
    budget: Duration
    turn_id: str | None
    call_id: str | None


@dataclass(frozen=True, slots=True)
class MessageStartEvent:
    """Describe the start of one streamed message item."""

    turn_id: str
    item_id: str
    role: Role
    index: int


@dataclass(frozen=True, slots=True)
class MessageUpdateEvent:
    """Carry one coalesced streamed message-part delta."""

    turn_id: str
    item_id: str
    part_index: int
    kind: PartKind
    delta: str
    coalesced: int
    total_chars: int


@dataclass(frozen=True, slots=True)
class MessageEndEvent:
    """Describe the end of one streamed message item."""

    turn_id: str
    item_id: str
    role: Role
    parts: int
    finish: FinishReason


@dataclass(frozen=True, slots=True)
class ItemCommittedEvent:
    """Describe one losslessly committed durable item."""

    event_index: int
    turn_id: str | None
    item: ItemRef


@dataclass(frozen=True, slots=True)
class CallOpenEvent:
    """Describe a speculative call once its target is known."""

    call_id: str
    target: CallTarget
    kind: TargetKind
    turn_id: str
    place: Place


@dataclass(frozen=True, slots=True)
class ToolCallEvent:
    """Carry one canonical, independently gateable tool invocation."""

    call_id: str
    invocation_id: str
    target: CallTarget
    kind: TargetKind
    args: Mapping[str, Any]
    raw_args: bytes
    repaired: bool
    turn_id: str
    session_id: str
    cwd: EnvPath
    origin: CallOrigin
    batch: tuple[CallRef, ...]
    deadline: Duration | None
    bash: BashIR | None


@dataclass(frozen=True, slots=True)
class ToolExecutionStartEvent:
    """Describe the start of an admitted tool execution."""

    call_id: str
    invocation_id: str
    target: CallTarget
    place: Place
    deadline: Duration | None


@dataclass(frozen=True, slots=True)
class ToolUpdateEvent:
    """Carry one coalesced structured tool-progress update."""

    call_id: str
    target: CallTarget
    update: Mapping[str, Any]
    coalesced: int


@dataclass(frozen=True, slots=True)
class ToolExecutionEndEvent:
    """Describe executor settlement before outcome projection."""

    call_id: str
    target: CallTarget
    outcome: OutcomeKind
    duration: Duration
    spilled: bool
    artifact: ArtifactUrl | None
    effects_unknown: bool


@dataclass(frozen=True, slots=True)
class ToolResultEvent:
    """Carry one durable tool outcome for annotation or spill advice."""

    call_id: str
    target: CallTarget
    outcome: OutcomeKind
    payload: Mapping[str, Any] | None
    fault: Mapping[str, Any] | None
    abort: Mapping[str, Any] | None
    artifact: ArtifactUrl | None
    useless: bool
    annotate: tuple[Annotation, ...] = ()
    spill: bool | None = None


@dataclass(frozen=True, slots=True)
class ToolApprovalRequestedEvent:
    """Describe the filing of a durable tool approval ticket."""

    call_id: str
    ticket_id: str
    target: CallTarget
    reasons: tuple[str, ...]
    requested_by: str


@dataclass(frozen=True, slots=True)
class ToolApprovalResolvedEvent:
    """Describe the durable resolution of a tool approval ticket."""

    call_id: str
    ticket_id: str
    target: CallTarget
    approved: bool
    reason: str | None
    resolved_by: str
    waited: Duration


@dataclass(frozen=True, slots=True)
class DeviceListEvent:
    """Carry the effective device set at one discovery boundary."""

    devices: tuple[DeviceInfo, ...]
    turn_id: str | None


@dataclass(frozen=True, slots=True)
class UserInputEvent:
    """Carry one user submission before it is journaled."""

    text: str
    images: tuple[BlobRef, ...]
    source: InputSource
    session_id: str
    pasted: bool


@dataclass(frozen=True, slots=True)
class UserBashEvent:
    """Carry one direct user shell command before execution."""

    command: str
    cwd: EnvPath
    exclude_from_context: bool
    bash: BashIR | None
    env_overrides: Mapping[str, str]


@dataclass(frozen=True, slots=True)
class UserEvalEvent:
    """Carry one direct user evaluation before execution."""

    code: str
    language: EvalLanguage
    cwd: EnvPath
    exclude_from_context: bool


@dataclass(frozen=True, slots=True)
class CommandInvokeEvent:
    """Carry one parsed extension-command invocation."""

    name: str
    argv: tuple[str, ...]
    raw: str
    mode: InvocationMode
    source: InputSource


@dataclass(frozen=True, slots=True)
class ResourcesDiscoverEvent:
    """Carry one gateable extension-resource discovery result."""

    reason: DiscoverReason
    root: EnvPath
    found: tuple[ResourceRef, ...]
    add: tuple[ResourceRef, ...] = ()
    keep: frozenset[str] | None = None


@dataclass(frozen=True, slots=True)
class ResourcesChangedEvent:
    """Describe a committed change to discovered extension resources."""

    added: tuple[ResourceRef, ...]
    removed: tuple[ResourceRef, ...]
    reason: DiscoverReason


@dataclass(frozen=True, slots=True)
class CapabilityBudgetEvent:
    """Describe constrained-sampling intents granted, degraded, or refused."""

    turn_id: str
    granted: tuple[Intent, ...]
    degraded: tuple[Intent, ...]
    refused: tuple[Intent, ...]


@dataclass(frozen=True, slots=True)
class ModelChangedEvent:
    """Describe a selected-model transition."""

    from_model: ModelRef | None
    to_model: ModelRef
    role: str
    reason: ModelChangeReason


@dataclass(frozen=True, slots=True)
class CredentialDisabledEvent:
    """Describe a provider credential disabled by the router."""

    provider: str
    account: str | None
    cause: str


@dataclass(frozen=True, slots=True)
class JobRegisteredEvent:
    """Describe registration of one detached durable job."""

    job_id: str
    owner: str
    call_id: str | None
    lifetime: ArtifactLifetime
    expected_artifact: ArtifactUrl | None


@dataclass(frozen=True, slots=True)
class JobSettledEvent:
    """Describe settlement of one detached durable job."""

    job_id: str
    owner: str
    artifact: ArtifactUrl | None
    failed: bool
    duration: Duration


@dataclass(frozen=True, slots=True)
class ExtensionActivateEvent:
    """Describe activation or reactivation of a lazy extension."""

    extension: str
    reason: ActivateReason
    session_started_at: datetime
    generation: int
    trigger: str | None


@dataclass(frozen=True, slots=True)
class ExtensionLoadEvent:
    """Describe loading one extension build into a host."""

    extension: str
    version: str
    source: str
    trust: Trust
    reloaded: bool

@dataclass(frozen=True, slots=True)
class ExtensionUnloadEvent:
    """Describe unloading one extension from a host."""

    extension: str
    reason: UnloadReason
    pending_hooks: int


@dataclass(frozen=True, slots=True)
class HostReconnectEvent:
    """Describe reconnection to a replacement CONTROL host generation."""

    generation: int
    missed_events: int
    restart_cause: str
    uptime: Duration


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
    "before_request", "models_discover", "provider_error", "provider_usage",
    "capability_budget", "model_changed", "credential_disabled", "compaction",
    "compaction_done", "thread_projection", "subagent_spawn", "worker_state",
    "job_registered", "job_settled", "extension_activate", "extension_load",
    "extension_unload", "host_reconnect",
)
EVENT_IDS: Final[Mapping[str, int]] = MappingProxyType(
    {name: event_id for event_id, name in enumerate(_EVENT_NAMES)}
)
"""Stable dense event identifiers used by the subscription bitmap."""


__all__ = tuple(
    name for name, value in globals().items()
    if name == "EVENT_IDS"
    or (isinstance(value, type) and value.__module__ == __name__)
)
