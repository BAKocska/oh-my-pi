from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import Literal

from omp import Context, Duration, device
from omp.agents import (
    AgentGone,
    AgentKind,
    Budget,
    DeliveryMode,
    SubagentSpec,
    Usage,
    get as get_agent,
    list as list_agents,
    spawn_all,
)


@dataclass(frozen=True, slots=True)
class SwarmMember:
    """One child declaration and its finite subtree budget."""

    task: str
    name: str | None = None
    agent: str = "task"
    model: str | None = None
    detached: bool = False
    max_depth: int = 1
    max_requests: int = 8
    max_input_tokens: int = 200_000
    max_output_tokens: int = 40_000
    max_usd: float = 5.0
    max_wall: str = "20m"


@dataclass(frozen=True, slots=True)
class SwarmArgs:
    """Arguments for running, inspecting, steering, or harvesting a swarm."""

    op: Literal["run", "status", "steer", "harvest"]
    members: tuple[SwarmMember, ...] = ()
    refs: tuple[str, ...] = ()
    ref: str | None = None
    text: str | None = None


@dataclass(frozen=True, slots=True)
class SpawnedMember:
    """Addressable identity returned for one admitted child."""

    run_id: str
    session_id: str
    name: str
    agent: str
    detached: bool
    output_url: str
    transcript_url: str


@dataclass(frozen=True, slots=True)
class RunResult:
    """One atomically admitted or queued spawn wave."""

    members: tuple[SpawnedMember, ...]


@dataclass(frozen=True, slots=True)
class RosterMember:
    """Compact live roster projection for one subagent."""

    id: str
    name: str
    status: str
    agent: str
    parent: str | None
    depth: int
    activity: str
    usage: Usage
    output_url: str
    transcript_url: str


@dataclass(frozen=True, slots=True)
class StatusResult:
    """Current core-owned subagent roster."""

    members: tuple[RosterMember, ...]


@dataclass(frozen=True, slots=True)
class SteerResult:
    """Delivery receipt or terminal-child transcript recovery pointer."""

    delivered: bool
    receipt: str | None = None
    transcript_url: str | None = None


@dataclass(frozen=True, slots=True)
class HarvestedMember:
    """Terminal child result with recursively summed usage."""

    run_id: str
    session_id: str
    name: str
    status: str
    text: str
    data: object | None
    fault: object | None
    turns: int
    model: str
    subtree_usage: Usage
    output_url: str
    transcript_url: str


@dataclass(frozen=True, slots=True)
class HarvestResult:
    """Terminal results collected for requested child references."""

    members: tuple[HarvestedMember, ...]


def _spec(member: SwarmMember) -> SubagentSpec:
    budget = Budget(
        max_requests=member.max_requests,
        max_input_tokens=member.max_input_tokens,
        max_output_tokens=member.max_output_tokens,
        max_usd=member.max_usd,
        max_wall=Duration(member.max_wall),
    )
    return SubagentSpec(
        task=member.task,
        name=member.name,
        agent=member.agent,
        model=member.model,
        background=member.detached,
        max_depth=member.max_depth,
        budget=budget,
    )


async def _harvest(ref: str) -> HarvestedMember:
    result = await (await get_agent(ref)).wait()
    return HarvestedMember(
        run_id=result.run_id,
        session_id=result.session_id,
        name=result.name,
        status=result.status.value,
        text=result.text,
        data=result.data,
        fault=result.fault,
        turns=result.turns,
        model=result.model,
        subtree_usage=result.subtree_usage,
        output_url=str(result.output_url),
        transcript_url=str(result.transcript_url),
    )


@device("swarm", family="agents", rev=1, place="host")
async def swarm(
    args: SwarmArgs, ctx: Context
) -> RunResult | StatusResult | SteerResult | HarvestResult:
    """Run one subagent wave or inspect, steer, and harvest its children."""

    del ctx
    if args.op == "run":
        if not args.members:
            raise ValueError("run requires at least one member")
        specs = tuple(_spec(member) for member in args.members)
        handles = await spawn_all(specs)
        return RunResult(
            members=tuple(
                SpawnedMember(
                    run_id=handle.run_id,
                    session_id=handle.session_id,
                    name=handle.name,
                    agent=handle.agent,
                    detached=member.detached,
                    output_url=str(handle.output_url),
                    transcript_url=str(handle.transcript_url),
                )
                for member, handle in zip(args.members, handles, strict=True)
            )
        )

    if args.op == "status":
        roster = await list_agents(kind=AgentKind.SUB)
        return StatusResult(
            members=tuple(
                RosterMember(
                    id=member.id,
                    name=member.name,
                    status=member.status.value,
                    agent=member.agent,
                    parent=member.parent,
                    depth=member.depth,
                    activity=member.activity,
                    usage=member.usage,
                    output_url=str(member.output_url),
                    transcript_url=str(member.transcript_url),
                )
                for member in roster
            )
        )

    if args.op == "steer":
        if args.ref is None or args.text is None:
            raise ValueError("steer requires ref and text")
        try:
            receipt = await (await get_agent(args.ref)).steer(
                args.text, mode=DeliveryMode.STEER
            )
        except AgentGone as gone:
            return SteerResult(
                delivered=False,
                transcript_url=str(gone.transcript_url),
            )
        return SteerResult(delivered=True, receipt=receipt.value)

    if args.op == "harvest":
        if not args.refs:
            raise ValueError("harvest requires at least one ref")
        return HarvestResult(
            members=tuple(await asyncio.gather(*(_harvest(ref) for ref in args.refs)))
        )

    raise ValueError(f"unsupported swarm operation: {args.op!r}")
