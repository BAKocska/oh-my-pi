from __future__ import annotations

import math
from dataclasses import dataclass
from typing import Literal

from omp import Duration, device
from omp.agents import (
    Cron,
    DeliveryMode,
    Inject,
    MissedRunPolicy,
    ScheduleBudget,
    ScheduleScope,
    Spawn,
    SubagentSpec,
    schedule,
    schedules,
)


@dataclass(frozen=True, slots=True)
class SchedulePromptArgs:
    """Arguments for one durable cron schedule upsert."""

    name: str
    prompt: str
    cron: str
    timezone: str = "UTC"
    scope: Literal["session", "project"] = "session"
    isolated: bool = False
    max_usd_per_firing: float | None = None
    budget_window: str = "30d"


@dataclass(frozen=True, slots=True)
class SchedulePromptResult:
    """Identity and delivery mode of an upserted schedule."""

    id: str
    name: str
    scope: str
    target: Literal["inject", "spawn"]


@dataclass(frozen=True, slots=True)
class SchedulesListArgs:
    """Optional scope filter for schedule inspection."""

    scope: Literal["all", "session", "project"] = "all"


@dataclass(frozen=True, slots=True)
class ScheduleView:
    """Stable, compact view of a declared durable schedule."""

    id: str
    name: str
    trigger: str
    scope: str
    enabled: bool
    next_ms: int | None
    fire_count: int
    miss_count: int


@dataclass(frozen=True, slots=True)
class SchedulesListResult:
    """Declared schedules owned by this extension."""

    schedules: list[ScheduleView]


def _scope(value: Literal["session", "project"]):
    return ScheduleScope.PROJECT if value == "project" else ScheduleScope.SESSION


@device("schedule_prompt", family="schedule", rev=1, place="host")
async def schedule_prompt(
    args: SchedulePromptArgs, ctx: omp.Context
) -> SchedulePromptResult:
    """Upsert a cron-backed injected prompt or isolated background task."""

    del ctx
    trigger = Cron(args.cron, tz=args.timezone)
    scope = _scope(args.scope)
    budget = None

    if args.isolated:
        if scope is ScheduleScope.PROJECT:
            amount = args.max_usd_per_firing
            if amount is None or not math.isfinite(amount) or amount <= 0:
                raise ValueError(
                    "project-scoped isolated schedules require a positive "
                    "max_usd_per_firing"
                )
            budget = ScheduleBudget(
                max_usd_per_firing=amount,
                window=Duration(args.budget_window),
            )
        delivery = Spawn(SubagentSpec(task=args.prompt, background=True))
        target: Literal["inject", "spawn"] = "spawn"
    else:
        # The documented Inject target has no prompt payload field; the core can
        # schedule this delivery, but cannot yet carry args.prompt with it.
        delivery = Inject(
            mode=DeliveryMode.NEXT_TURN,
            visible=True,
        )
        target = "inject"

    handle = await schedule(
        args.name,
        trigger,
        delivery,
        scope=scope,
        missed=MissedRunPolicy.COALESCE,
        overlap="skip",
        budget=budget,
    )
    return SchedulePromptResult(
        id=handle.id,
        name=handle.name,
        scope=args.scope,
        target=target,
    )


@device("schedules_list", family="schedule", rev=1, place="host")
async def schedules_list(
    args: SchedulesListArgs, ctx: omp.Context
) -> SchedulesListResult:
    """Read this extension's schedules from the core scheduler projection."""

    del ctx
    scope = None if args.scope == "all" else _scope(args.scope)
    declared = await schedules(scope=scope)
    return SchedulesListResult(
        schedules=[
            ScheduleView(
                id=schedule.id,
                name=schedule.name,
                trigger=str(schedule.trigger),
                scope=schedule.scope.value,
                enabled=schedule.enabled,
                next_ms=schedule.next_ms,
                fire_count=schedule.fire_count,
                miss_count=schedule.miss_count,
            )
            for schedule in declared
        ]
    )
