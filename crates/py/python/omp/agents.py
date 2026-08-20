"""Frozen agent completions, continuation decisions, and durable schedules."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from enum import StrEnum
from typing import Any, Literal, TypeAlias

from _omp import Duration
from ._errors import NotWiredError


@dataclass(frozen=True, slots=True)
class Usage:
    """Token and cost usage attributed to one completion."""

    input: int = 0
    output: int = 0
    cache_read: int = 0
    cache_write: int = 0
    reasoning: int = 0
    cost_usd: float = 0.0


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
    """Request a budgeted, stateless completion through the future host arm."""

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


async def loop_signal() -> LoopSignal:
    """Read the Core's current conservative loop-stall signal."""

    raise NotWiredError("omp.agents.loop_signal")


class DeliveryMode(StrEnum):
    """When an injected item becomes visible to the target agent."""

    ASIDE = "aside"
    STEER = "steer"
    NEXT_TURN = "next_turn"


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
    """Fixed-interval trigger with optional jitter and wall-clock alignment."""

    interval: Duration
    jitter: Duration = Duration("0s")
    align: bool = False


@dataclass(frozen=True, slots=True)
class At:
    """One-shot trigger at an absolute Unix epoch millisecond."""

    epoch_ms: int


@dataclass(frozen=True, slots=True)
class AfterIdle:
    """Trigger armed after an agent has remained settled for a duration."""

    idle: Duration


Trigger: TypeAlias = Cron | Every | At | AfterIdle


@dataclass(frozen=True, slots=True)
class SubagentSpec:
    """Frozen declaration of a scheduled or directly spawned child."""

    task: str
    name: str | None = None
    agent: str = "task"
    system_prompt: str | None = None
    model: str | None = None
    background: bool = False
    max_depth: int = 1
    labels: Mapping[str, str] = field(default_factory=dict)


@dataclass(frozen=True, slots=True)
class Inject:
    """Deliver a scheduled prompt to the declaring agent."""

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
    window: Duration = Duration("30d")
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
class ScheduleHandle:
    """Identity returned after a durable schedule upsert."""

    id: str
    name: str


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
    """Upsert a durable schedule through the future scheduler host arm."""

    del name, trigger, delivery, scope, missed, overlap, upgrade, budget
    raise NotWiredError("omp.agents.schedule")


async def schedules(
    *, scope: ScheduleScope | None = None, owner: str | None = None
) -> list[Schedule]:
    """List visible durable schedules."""
    del scope, owner
    raise NotWiredError("omp.agents.schedules")


async def unschedule(name_or_id: str) -> bool:
    """Delete a schedule by owner-local name or stable identifier."""

    del name_or_id
    raise NotWiredError("omp.agents.unschedule")

__all__ = (
    "AfterIdle", "At", "Completion", "Continue", "ContinuationLedger", "Cron",
    "Delivery", "DeliveryMode", "Every", "Inject", "LoopSignal", "MissedRunPolicy",
    "Schedule", "ScheduleBudget", "ScheduleHandle", "ScheduleScope", "Settle", "Spawn",
    "SubagentSpec", "Trigger", "UpgradePolicy", "Usage", "completion", "continuations",
    "loop_signal", "schedule", "schedules", "unschedule",
)
