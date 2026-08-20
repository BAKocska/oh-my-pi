"""Frozen agent completions, supervision, messaging, and scheduling."""

from __future__ import annotations

import asyncio
import builtins as _builtins
import re
from collections.abc import Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass, field
from enum import StrEnum
from typing import Literal, TypeAlias

from _omp import (
    AgentUrl,
    ArtifactUrl,
    Duration,
    EnvPath,
    HistoryUrl,
    OmpError,
    WorkspaceUri,
)

from . import limits as _limits
from . import Fault
from ._errors import NotWiredError
from .policy import PolicyDenied


DEFAULT_MAX_DEPTH = 2
DEFAULT_MAX_CONCURRENCY = 32
DEFAULT_CONTINUATION_CAP = _limits.SETTLE_CONTINUATION_CAP
MAILBOX_CAPACITY = 100
STEER_GRACE = Duration("500ms")
MIN_SCHEDULE_INTERVAL = Duration("30s")
MAX_BACKFILL = 32
EMPTY_OUTPUT_RETRY_CAP = 3

depth: int = 0


class AgentsError(OmpError):
    """Base error for agent operations."""


class SpawnDenied(AgentsError):
    """Raised when a child declaration cannot be admitted."""

    def __init__(self, reason: str, field: str | None = None) -> None:
        self.reason = reason
        self.field = field
        super().__init__(f"spawn denied: {reason}" + (f" (field: {field})" if field else ""))


class DepthExceeded(AgentsError):
    """Raised when the agent tree is already at its depth ceiling."""

    def __init__(self, depth: int, max_depth: int) -> None:
        self.depth = depth
        self.max_depth = max_depth
        super().__init__(f"agent depth {depth} exceeds maximum {max_depth}")


class ConcurrencyExhausted(AgentsError):
    """Raised when both child execution and admission queues are full."""

    def __init__(self, running: int, queued: int, max_concurrency: int) -> None:
        self.running = running
        self.queued = queued
        self.max_concurrency = max_concurrency
        super().__init__(
            f"agent concurrency exhausted: {running} running, {queued} queued, "
            f"maximum {max_concurrency}"
        )


class AgentGone(AgentsError):
    """Raised when an agent is terminal or tombstoned."""

    def __init__(self, ref: str, status: AgentStatus, transcript_url: str) -> None:
        self.ref = ref
        self.status = status
        self.transcript_url = transcript_url
        super().__init__(
            f"agent {ref!r} is {status.value}; transcript: {transcript_url}"
        )


class RewindPending(AgentsError):
    """Raised when a rewind encounters a durable turn without a receipt."""

    def __init__(self, turn_id: str) -> None:
        self.turn_id = turn_id
        super().__init__(f"turn {turn_id!r} is still pending")


class SnapshotUnsupported(AgentsError):
    """Raised when the environment cannot snapshot its workspace."""

    def __init__(self, capability: str = "env:workspace.snapshot") -> None:
        self.capability = capability
        super().__init__(f"snapshot capability unavailable: {capability}")


class ScheduleRejected(AgentsError):
    """Raised when a durable schedule declaration is invalid."""

    def __init__(self, reason: str, field: str | None = None) -> None:
        self.reason = reason
        self.field = field
        super().__init__(
            f"schedule rejected: {reason}" + (f" (field: {field})" if field else "")
        )


class CompletionFailed(AgentsError):
    """Raised when a one-shot completion cannot produce an accepted result."""

    def __init__(self, reason: str, raw: str | None, usage: Usage) -> None:
        self.reason = reason
        self.raw = raw
        self.usage = usage
        super().__init__(f"completion failed: {reason}")


@dataclass(frozen=True, slots=True)
class Usage:
    """Token, request, cost, and wall-time usage for an agent node."""

    input_tokens: int = 0
    cached_input_tokens: int = 0
    output_tokens: int = 0
    reasoning_tokens: int = 0
    cache_write_tokens: int = 0
    requests: int = 0
    cost_usd: float = 0.0
    wall: Duration = Duration("0s")


@dataclass(frozen=True, slots=True)
class Completion:
    """Settled output from one stateless completion request."""

    text: str
    choice: str | None
    data: object | None
    usage: Usage
    model: str
    fell_back: bool = False
    fault: object | None = None


_DEFAULT = object()


async def completion(
    prompt: object,
    *,
    role: str = "smol",
    system: str | None = None,
    choices: Sequence[str] | None = None,
    schema: Mapping[str, object] | None = None,
    default: object = _DEFAULT,
    scope: Literal["turn", "session"] = "turn",
    max_output_tokens: int | None = None,
    deadline: Duration = Duration("10s"),
    labels: Mapping[str, str] | None = None,
) -> Completion:
    """Request a budgeted, stateless completion through the host."""
    del prompt, role, system, choices, schema, default, scope, max_output_tokens, deadline, labels
    raise NotWiredError("omp.agents.completion")


@dataclass(frozen=True, slots=True)
class Continue:
    """Decline settlement by supplying the next continuation item."""

    prompt: str
    visible: bool = False
    role: Literal["user", "system"] = "system"
    label: str | None = None
    collapse_prior: bool = True


@dataclass(frozen=True, slots=True)
class Settle:
    """Explicitly accept settlement without another turn."""


@dataclass(frozen=True, slots=True)
class ContinuationPolicy:
    """Per-extension recursive continuation policy."""

    max_consecutive: int = DEFAULT_CONTINUATION_CAP
    max_total: int | None = None
    min_interval: Duration = Duration("0s")
    on_exhausted: Literal["settle", "notify"] = "notify"


@dataclass(frozen=True, slots=True)
class ContinuationLedger:
    """Durable view of the recursive continuation budget."""

    consecutive: int
    total: int
    cap: int
    last_ms: int
    refusals: int
    owner: str | None = None


@dataclass(frozen=True, slots=True)
class LoopSignal:
    """Core-owned repetition and progress facts for an autonomous loop."""

    repeats: int
    digest: str
    no_progress_turns: int
    empty_output_retries: int
    stalled: bool


async def continuations() -> ContinuationLedger:
    """Read the current recursive continuation ledger."""
    raise NotWiredError("omp.agents.continuations")


async def set_continuation_policy(policy: ContinuationPolicy) -> None:
    """Set this extension's continuation policy."""
    del policy
    raise NotWiredError("omp.agents.set_continuation_policy")


async def loop_signal() -> LoopSignal:
    """Read the Core's current conservative loop-stall signal."""
    raise NotWiredError("omp.agents.loop_signal")


class DeliveryMode(StrEnum):
    """When an injected item becomes visible to the target agent."""

    ASIDE = "aside"
    STEER = "steer"
    NEXT_TURN = "next_turn"


class Isolation(StrEnum):
    """How much parent conversation a child inherits."""

    CLEAN = "clean"
    FORK = "fork"
    FILTERED = "filtered"


class ThinkingLevel(StrEnum):
    """Coarse reasoning level requested for a child."""

    OFF = "off"
    LO = "lo"
    MED = "med"
    HI = "hi"


class MergeMode(StrEnum):
    """Disposition of a worktree-isolated child's changes."""

    NONE = "none"
    BRANCH = "branch"
    PATCH = "patch"


@dataclass(frozen=True, slots=True)
class Budget:
    """Hard resource ceilings for one child and its subtree."""

    max_requests: int | None = None
    max_input_tokens: int | None = None
    max_output_tokens: int | None = None
    max_usd: float | None = None
    max_wall: Duration | None = None


_NAME_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_]{0,31}$")


@dataclass(frozen=True, slots=True)
class SubagentSpec:
    """Complete frozen declaration of a child agent."""

    task: str
    name: str | None = None
    agent: str = "task"
    system_prompt: str | None = None
    model: str | None = None
    on_model_unavailable: Literal["fail", "parent"] = "fail"
    thinking: ThinkingLevel | None = None
    allowed_devices: frozenset[str] | None = None
    disallowed_devices: frozenset[str] = frozenset()
    isolation: Isolation = Isolation.CLEAN
    max_depth: int = 1
    cwd: EnvPath | None = None
    worktree: bool = False
    merge: MergeMode = MergeMode.NONE
    env_vars: Mapping[str, str] = field(default_factory=dict)
    background: bool = False
    output_schema: Mapping[str, object] | None = None
    schema_mode: Literal["permissive", "strict"] = "permissive"
    deadline: Duration | None = None
    request_budget: int | None = None
    budget: Budget | None = None
    labels: Mapping[str, str] = field(default_factory=dict)

    def __post_init__(self) -> None:
        """Validate identity fields that require no host state."""
        if not self.task.strip():
            raise SpawnDenied("task must be non-empty", field="task")
        if self.name is not None and _NAME_RE.fullmatch(self.name) is None:
            raise SpawnDenied(
                "name must match ^[A-Za-z][A-Za-z0-9_]{0,31}$", field="name"
            )


class RunStatus(StrEnum):
    """Lifecycle state of a supervised child run."""

    PENDING = "pending"
    RUNNING = "running"
    SETTLED = "settled"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"
    EXHAUSTED = "exhausted"

    @property
    def terminal(self) -> bool:
        """Whether this state is terminal."""
        return self in {
            RunStatus.COMPLETED,
            RunStatus.FAILED,
            RunStatus.CANCELLED,
            RunStatus.EXHAUSTED,
        }


@dataclass(frozen=True, slots=True)
class Progress:
    """Sanitized render snapshot of a child run's progress."""

    status: RunStatus
    turns: int
    requests: int
    tool_calls: int
    context_tokens: int
    context_window: int
    usage: Usage
    activity: str
    model: str
    last_activity_ms: int


@dataclass(frozen=True, slots=True)
class WorktreeOutcome:
    """Disposition and recovery details for a child's worktree."""

    path: EnvPath
    merge: MergeMode
    applied: bool
    branch: str | None
    patch_url: ArtifactUrl | None
    conflicts: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class SubagentResult:
    """Terminal result and durable locations for a child run."""

    run_id: str
    session_id: str
    name: str
    status: RunStatus
    text: str
    data: object | None
    fault: Fault | None
    usage: Usage
    subtree_usage: Usage
    turns: int
    model: str
    model_fallback: bool
    warnings: tuple[str, ...]
    output_url: AgentUrl
    transcript_url: HistoryUrl
    worktree: WorktreeOutcome | None


class Receipt(StrEnum):
    """Delivery disposition for an inter-agent message."""

    DELIVERED = "delivered"
    WOKEN = "woken"
    REVIVED = "revived"
    BUFFERED = "buffered"
    FAILED = "failed"


class SubagentHandle:
    """Live CONTROL handle over a supervised child run."""
    run_id: str
    session_id: str
    name: str
    agent: str
    depth: int
    effective_max_depth: int
    spec: SubagentSpec
    worktree_path: EnvPath | None
    output_url: AgentUrl
    transcript_url: HistoryUrl


    def __init__(
        self,
        run_id: str,
        session_id: str,
        name: str,
        agent: str,
        depth: int,
        effective_max_depth: int,
        spec: SubagentSpec,
        worktree_path: EnvPath | None,
        output_url: AgentUrl,
        transcript_url: HistoryUrl,
    ) -> None:
        self.run_id = run_id
        self.session_id = session_id
        self.name = name
        self.agent = agent
        self.depth = depth
        self.effective_max_depth = effective_max_depth
        self.spec = spec
        self.worktree_path = worktree_path
        self.output_url = output_url
        self.transcript_url = transcript_url
        self._released = False

    async def status(self) -> RunStatus:
        """Read the child's current lifecycle state."""
        raise NotWiredError("omp.agents.SubagentHandle.status")

    async def progress(self) -> Progress:
        """Read a sanitized progress snapshot."""
        raise NotWiredError("omp.agents.SubagentHandle.progress")

    async def steer(
        self, text: str, *, mode: DeliveryMode = DeliveryMode.ASIDE
    ) -> Receipt:
        """Post a message into the child's mailbox."""
        del text, mode
        raise NotWiredError("omp.agents.SubagentHandle.steer")

    async def cancel(
        self,
        *,
        reason: str = "cancelled by extension",
        grace: Duration = STEER_GRACE,
    ) -> None:
        """Cancel the child and its structural resources."""
        del reason, grace
        raise NotWiredError("omp.agents.SubagentHandle.cancel")

    async def wait(self, *, timeout: Duration | None = None) -> SubagentResult:
        """Wait for a terminal child result."""
        del timeout
        raise NotWiredError("omp.agents.SubagentHandle.wait")

    async def result(self) -> SubagentResult | None:
        """Return the terminal result without blocking, when available."""
        raise NotWiredError("omp.agents.SubagentHandle.result")

    async def release(self) -> None:
        """Relinquish structural ownership of the child."""
        self._released = True
        raise NotWiredError("omp.agents.SubagentHandle.release")

    async def __aenter__(self) -> SubagentHandle:
        """Enter structural ownership of this child."""
        return self

    async def __aexit__(self, exc_type: object, exc: object, tb: object) -> None:
        """Cancel an unreleased child when leaving its ownership scope."""
        del exc_type, exc, tb
        if not self._released:
            await self.cancel()


async def spawn(spec: SubagentSpec) -> SubagentHandle:
    """Admit and start one child agent."""
    del spec
    raise NotWiredError("omp.agents.spawn")


async def spawn_all(specs: Sequence[SubagentSpec]) -> _builtins.list[SubagentHandle]:
    """Atomically admit and start a batch of child agents."""
    del specs
    raise NotWiredError("omp.agents.spawn_all")


class AgentKind(StrEnum):
    """Kind of agent represented by a roster row."""

    MAIN = "main"
    SUB = "sub"
    ADVISOR = "advisor"


class AgentStatus(StrEnum):
    """Roster lifecycle state of an agent session."""

    RUNNING = "running"
    IDLE = "idle"
    PARKED = "parked"
    ABORTED = "aborted"


@dataclass(frozen=True, slots=True)
class AgentRef:
    """Addressable roster snapshot for an agent."""

    id: str
    name: str
    kind: AgentKind
    status: AgentStatus
    agent: str
    parent: str | None
    depth: int
    activity: str
    last_activity_ms: int
    usage: Usage
    output_url: AgentUrl
    transcript_url: HistoryUrl


@dataclass(frozen=True, slots=True)
class SpawnLimits:
    """Snapshot of every ceiling that can refuse another spawn."""

    max_depth: int
    depth: int
    max_concurrency: int
    running: int
    queued: int
    continuation_cap: int
    continuations_used: int
    spawn_allowed: bool


async def get(ref: str) -> SubagentHandle:
    """Resolve an agent reference to a live handle."""
    del ref
    raise NotWiredError("omp.agents.get")


async def revive(ref: str) -> SubagentHandle:
    """Cold-revive a parked child session."""
    del ref
    raise NotWiredError("omp.agents.revive")


async def limits() -> SpawnLimits:
    """Read current child-spawn ceilings."""
    raise NotWiredError("omp.agents.limits")




@dataclass(frozen=True, slots=True)
class Message:
    """One inter-agent mailbox message."""

    id: str
    from_: str
    to: str
    text: str
    mode: DeliveryMode
    reply_to: str | None
    sent_ms: int
    session_id: str


async def send(
    to: str,
    text: str,
    *,
    mode: DeliveryMode = DeliveryMode.ASIDE,
    reply_to: str | None = None,
    await_reply: bool = False,
    timeout: Duration = Duration("60s"),
) -> Receipt | Message:
    """Send a message to an addressable agent."""
    del to, text, mode, reply_to, await_reply, timeout
    raise NotWiredError("omp.agents.send")


async def broadcast(
    text: str,
    *,
    scope: Literal["session", "project"] = "session",
    mode: DeliveryMode = DeliveryMode.ASIDE,
) -> dict[str, Receipt]:
    """Send a message to every agent in a scope."""
    del text, scope, mode
    raise NotWiredError("omp.agents.broadcast")


async def inbox(*, peek: bool = False, limit: int | None = None) -> _builtins.list[Message]:
    """Drain or inspect this agent's buffered mailbox."""
    del peek, limit
    raise NotWiredError("omp.agents.inbox")


async def wait_for(
    *,
    sender: str | None = None,
    reply_to: str | None = None,
    timeout: Duration = Duration("60s"),
) -> Message | None:
    """Wait for a matching inter-agent message."""
    del sender, reply_to, timeout
    raise NotWiredError("omp.agents.wait_for")


async def peers(
    *, scope: Literal["session", "project"] = "session"
) -> _builtins.list[AgentRef]:
    """List messageable peers in a scope."""
    del scope
    raise NotWiredError("omp.agents.peers")


async def inject(
    prompt: str,
    *,
    mode: DeliveryMode = DeliveryMode.NEXT_TURN,
    visible: bool = False,
    role: Literal["user", "system"] = "system",
) -> Receipt:
    """Inject an out-of-band item into this agent's mailbox."""
    del prompt, mode, visible, role
    raise NotWiredError("omp.agents.inject")


class RestoreScope(StrEnum):
    """Which state a rewind or restore operation affects."""

    THREAD = "thread"
    WORKSPACE = "workspace"
    BOTH = "both"


@dataclass(frozen=True, slots=True)
class RewindTarget:
    """Selectable live user-message point in the journal."""

    event: int
    keep: int | None
    text: str
    ts_ms: int
    snapshot_id: str | None


@dataclass(frozen=True, slots=True)
class Conflict:
    """Structured reason a workspace generation cannot be restored."""

    path: EnvPath
    reason: Literal[
        "open_lease", "modified_after_snapshot", "outside_root", "permission"
    ]
    lease_holder: str | None


@dataclass(frozen=True, slots=True)
class RestoreReport:
    """Workspace restore effects and recovery identity."""

    from_generation: int
    to_generation: int
    written: int
    deleted: int
    unchanged: int
    conflicts: tuple[Conflict, ...]
    undo_snapshot_id: str
    dry_run: bool


@dataclass(frozen=True, slots=True)
class RewindReport:
    """Atomic thread and optional workspace rewind report."""

    head: int
    dropped_items: int
    scope: RestoreScope
    restore: RestoreReport | None
    dry_run: bool


@dataclass(frozen=True, slots=True)
class Snapshot:
    """Content-addressed generation of a workspace."""

    id: str
    generation: int
    label: str | None
    created_ms: int
    root: WorkspaceUri
    parent: str | None
    tree_hash: str
    entry_count: int
    bytes: int
    partial: bool


async def rewind_targets() -> _builtins.list[RewindTarget]:
    """List live user-message rewind targets oldest first."""
    raise NotWiredError("omp.agents.rewind_targets")


async def rewind(
    to: int | None,
    *,
    scope: RestoreScope = RestoreScope.THREAD,
    snapshot_id: str | None = None,
    dry_run: bool = False,
) -> RewindReport:
    """Atomically rewind thread state and optionally workspace state."""
    del to, scope, snapshot_id, dry_run
    raise NotWiredError("omp.agents.rewind")


async def snapshot(
    *, label: str | None = None, paths: Sequence[str] | None = None
) -> Snapshot:
    """Capture a content-addressed workspace generation."""
    del label, paths
    raise NotWiredError("omp.agents.snapshot")


async def snapshots(*, limit: int = 50) -> _builtins.list[Snapshot]:
    """List workspace snapshots newest first."""
    del limit
    raise NotWiredError("omp.agents.snapshots")


async def restore(
    snapshot_id: str,
    *,
    paths: Sequence[str] | None = None,
    dry_run: bool = False,
) -> RestoreReport:
    """Restore files from a content-addressed workspace generation."""
    del snapshot_id, paths, dry_run
    raise NotWiredError("omp.agents.restore")


class MissedRunPolicy(StrEnum):
    """Recovery policy for firings missed while the scheduler was down."""

    SKIP = "skip"
    COALESCE = "coalesce"
    BACKFILL = "backfill"


class ScheduleScope(StrEnum):
    """Durability scope for a schedule declaration."""

    SESSION = "session"
    PROJECT = "project"


class UpgradePolicy(StrEnum):
    """Artifact selection policy for future schedule firings."""

    PINNED = "pinned"
    AUTO = "auto"


@dataclass(frozen=True, slots=True)
class Cron:
    """Cron trigger evaluated in an IANA timezone."""

    expr: str
    tz: str = "UTC"


@dataclass(frozen=True, slots=True)
class Every:
    """Fixed-interval trigger with optional jitter and alignment."""

    interval: Duration
    jitter: Duration = Duration("0s")
    align: bool = False


@dataclass(frozen=True, slots=True)
class At:
    """One-shot trigger at an absolute Unix epoch millisecond."""

    epoch_ms: int


@dataclass(frozen=True, slots=True)
class AfterIdle:
    """Trigger armed after an agent remains settled for a duration."""

    idle: Duration


Trigger: TypeAlias = Cron | Every | At | AfterIdle


@dataclass(frozen=True, slots=True)
class Inject:
    """Deliver a scheduled prompt to the declaring agent."""

    prompt: str
    mode: DeliveryMode = DeliveryMode.NEXT_TURN
    visible: bool = False


@dataclass(frozen=True, slots=True)
class Spawn:
    """Deliver a firing by spawning a supervised child."""

    spec: SubagentSpec


Delivery: TypeAlias = Inject | Spawn


@dataclass(frozen=True, slots=True)
class ScheduleBudget:
    """Hard request and cost ceilings for a durable schedule."""

    max_usd_per_firing: float | None = None
    max_usd_per_window: float | None = None
    window: Duration = Duration("720h")
    max_requests_per_firing: int | None = None


@dataclass(frozen=True, slots=True)
class Schedule:
    """Frozen projection of one durable schedule."""

    id: str
    name: str
    trigger: Trigger
    delivery: Delivery
    scope: ScheduleScope
    enabled: bool
    owner: str
    principal: str
    artifact_digest: str
    upgrade: UpgradePolicy
    missed: MissedRunPolicy
    budget: ScheduleBudget | None
    overlap: Literal["skip", "queue"]
    created_ms: int
    next_ms: int | None
    last_ms: int | None
    fire_count: int
    miss_count: int


@dataclass(frozen=True, slots=True)
class Firing:
    """Durable outcome of one schedule firing."""

    schedule_id: str
    idempotency_key: str
    at_ms: int
    late_ms: int
    outcome: Literal[
        "injected", "spawned", "skipped", "failed", "duplicate", "budget_refused"
    ]
    artifact_digest: str
    principal: str
    run_id: str | None
    detail: str | None


class ScheduleHandle:
    """Live identity and control surface for a durable schedule."""
    id: str
    name: str


    def __init__(self, id: str, name: str) -> None:
        self.id = id
        self.name = name

    async def pause(self) -> None:
        """Pause future firings."""
        raise NotWiredError("omp.agents.ScheduleHandle.pause")

    async def resume(self) -> None:
        """Resume future firings."""
        raise NotWiredError("omp.agents.ScheduleHandle.resume")

    async def delete(self) -> None:
        """Delete this durable schedule."""
        raise NotWiredError("omp.agents.ScheduleHandle.delete")

    async def fire_now(self) -> Receipt:
        """Request a journaled manual firing."""
        raise NotWiredError("omp.agents.ScheduleHandle.fire_now")

    async def info(self) -> Schedule:
        """Read the current schedule projection."""
        raise NotWiredError("omp.agents.ScheduleHandle.info")

    async def history(self, limit: int = 20) -> _builtins.list[Firing]:
        """Read durable firing history."""
        del limit
        raise NotWiredError("omp.agents.ScheduleHandle.history")


async def schedule(
    name: str,
    trigger: Trigger,
    delivery: Delivery,
    *,
    scope: ScheduleScope = ScheduleScope.SESSION,
    missed: MissedRunPolicy = MissedRunPolicy.COALESCE,
    overlap: Literal["skip", "queue"] = "skip",
    upgrade: UpgradePolicy = UpgradePolicy.PINNED,
    budget: ScheduleBudget | None = None,
) -> ScheduleHandle:
    """Upsert a durable schedule through the scheduler host arm."""
    del name, trigger, delivery, scope, missed, overlap, upgrade, budget
    raise NotWiredError("omp.agents.schedule")


async def schedules(
    *, scope: ScheduleScope | None = None, owner: str | None = None
) -> _builtins.list[Schedule]:
    """List visible durable schedules."""
    del scope, owner
    raise NotWiredError("omp.agents.schedules")


async def unschedule(name_or_id: str) -> bool:
    """Delete a schedule by owner-local name or stable identifier."""
    del name_or_id
    raise NotWiredError("omp.agents.unschedule")


class TimerHandle:
    """Host-local cancellable timer handle."""

    def __init__(
        self,
        loop: asyncio.AbstractEventLoop,
        delay: float,
        callback: Callable[[], Awaitable[None]],
        repeat: bool,
    ) -> None:
        self._loop = loop
        self._delay = delay
        self._callback = callback
        self._repeat = repeat
        self._scheduled: asyncio.TimerHandle | None = None
        self._task: asyncio.Task[None] | None = None
        self._cancelled = False
        self._arm()

    def _arm(self) -> None:
        self._scheduled = self._loop.call_later(self._delay, self._fire)

    def _fire(self) -> None:
        self._scheduled = None
        if self._cancelled:
            return
        self._task = self._loop.create_task(self._run())

    async def _run(self) -> None:
        try:
            await self._callback()
        except BaseException:
            self._cancelled = True
            raise
        else:
            if self._repeat and not self._cancelled:
                self._arm()
        finally:
            self._task = None

    def cancel(self) -> None:
        """Cancel any pending firing or running callback."""
        self._cancelled = True
        if self._scheduled is not None:
            self._scheduled.cancel()
            self._scheduled = None
        if self._task is not None:
            self._task.cancel()

    @property
    def active(self) -> bool:
        """Whether the timer still has a pending or active firing."""
        return not self._cancelled and (
            self._scheduled is not None or self._task is not None
        )


def timer(
    delay: Duration,
    callback: Callable[[], Awaitable[None]],
    *,
    repeat: bool = False,
) -> TimerHandle:
    """Schedule a host-local asynchronous callback on the running event loop."""
    loop = asyncio.get_running_loop()
    return TimerHandle(loop, delay.seconds, callback, repeat)


async def list(
    *,
    kind: AgentKind | None = None,
    status: AgentStatus | None = None,
    include_parked: bool = True,
) -> _builtins.list[AgentRef]:
    """List visible agents in tree order."""
    del kind, status, include_parked
    raise NotWiredError("omp.agents.list")

__all__ = (
    "AfterIdle",
    "AgentGone",
    "AgentKind",
    "AgentRef",
    "AgentStatus",
    "AgentsError",
    "At",
    "Budget",
    "Completion",
    "CompletionFailed",
    "ConcurrencyExhausted",
    "Conflict",
    "Continue",
    "ContinuationLedger",
    "ContinuationPolicy",
    "Cron",
    "DEFAULT_CONTINUATION_CAP",
    "DEFAULT_MAX_CONCURRENCY",
    "DEFAULT_MAX_DEPTH",
    "Delivery",
    "DeliveryMode",
    "DepthExceeded",
    "EMPTY_OUTPUT_RETRY_CAP",
    "Every",
    "Firing",
    "Inject",
    "Isolation",
    "LoopSignal",
    "MAILBOX_CAPACITY",
    "MAX_BACKFILL",
    "MIN_SCHEDULE_INTERVAL",
    "MergeMode",
    "Message",
    "MissedRunPolicy",
    "PolicyDenied",
    "Progress",
    "Receipt",
    "RestoreReport",
    "RestoreScope",
    "RewindPending",
    "RewindReport",
    "RewindTarget",
    "RunStatus",
    "STEER_GRACE",
    "Schedule",
    "ScheduleBudget",
    "ScheduleHandle",
    "ScheduleRejected",
    "ScheduleScope",
    "Settle",
    "Snapshot",
    "SnapshotUnsupported",
    "Spawn",
    "SpawnDenied",
    "SpawnLimits",
    "SubagentHandle",
    "SubagentResult",
    "SubagentSpec",
    "ThinkingLevel",
    "TimerHandle",
    "Trigger",
    "UpgradePolicy",
    "Usage",
    "WorktreeOutcome",
    "broadcast",
    "completion",
    "continuations",
    "depth",
    "get",
    "inbox",
    "inject",
    "limits",
    "list",
    "loop_signal",
    "peers",
    "restore",
    "revive",
    "rewind",
    "rewind_targets",
    "schedule",
    "schedules",
    "send",
    "set_continuation_policy",
    "snapshot",
    "snapshots",
    "spawn",
    "spawn_all",
    "timer",
    "unschedule",
    "wait_for",
)
