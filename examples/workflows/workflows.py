from __future__ import annotations

import asyncio
import hashlib
import json
from collections.abc import Mapping
from dataclasses import asdict, dataclass
from typing import Literal

import omp


_TerminalStatus = Literal["completed", "failed", "cancelled", "exhausted"]


@dataclass(frozen=True, slots=True)
class WorkflowBudget:
    """Hard limits applied to one workflow node and its subtree."""

    max_requests: int | None = 8
    max_input_tokens: int | None = 200_000
    max_output_tokens: int | None = 40_000
    max_usd: float | None = 5.0
    max_wall: str | None = "20m"


@dataclass(frozen=True, slots=True)
class WorkflowNode:
    """One declarative node compiled into an ``omp.agents.SubagentSpec``."""

    name: str
    task: str
    agent: str = "task"
    model: str | None = None
    system_prompt: str | None = None
    thinking: omp.agents.ThinkingLevel | None = None
    allowed_devices: tuple[str, ...] | None = None
    disallowed_devices: tuple[str, ...] = ()
    isolation: omp.agents.Isolation = omp.agents.Isolation.CLEAN
    max_depth: int = 0
    worktree: bool = False
    request_budget: int | None = None
    output_schema: Mapping[str, object] | None = None
    schema_mode: Literal["permissive", "strict"] = "permissive"
    budget: WorkflowBudget = WorkflowBudget()


@dataclass(frozen=True, slots=True)
class WorkflowEdge:
    """A dependency from ``upstream`` to ``downstream``."""

    upstream: str
    downstream: str


@dataclass(frozen=True, slots=True)
class WorkflowArgs:
    """A DAG supplied directly, or an empty request selecting the configured DAG."""

    name: str | None = None
    nodes: tuple[WorkflowNode, ...] = ()
    edges: tuple[WorkflowEdge, ...] = ()


@omp.entry_kind(
    "examples.workflows.node-settled", rev="v.1", display=False, spill=False
)
@dataclass(frozen=True, slots=True)
class WorkflowNodeSettled:
    """The durable terminal receipt for one workflow node."""

    workflow_id: str
    workflow: str
    node: str
    status: _TerminalStatus
    run_id: str
    session_id: str
    output_url: str
    transcript_url: str
    detail: str | None = None


@omp.entry_kind(
    "examples.workflows.node-skipped", rev="v.1", display=False, spill=False
)
@dataclass(frozen=True, slots=True)
class WorkflowNodeSkipped:
    """A durable record that a failed dependency blocked one node."""

    workflow_id: str
    workflow: str
    node: str
    blocked_by: tuple[str, ...]
    reason: Literal["failed_dependency"] = "failed_dependency"


@dataclass(frozen=True, slots=True)
class WorkflowNodeOutcome:
    """The journal-folded outcome of one workflow node."""

    node: str
    status: str
    output_url: str | None = None
    blocked_by: tuple[str, ...] = ()
    resumed: bool = False


@dataclass(frozen=True, slots=True)
class WorkflowResult:
    """The final journal-backed projection of a workflow run."""

    workflow_id: str
    workflow: str
    outcomes: tuple[WorkflowNodeOutcome, ...]


def _budget(value: WorkflowBudget) -> omp.agents.Budget:
    return omp.agents.Budget(
        max_requests=value.max_requests,
        max_input_tokens=value.max_input_tokens,
        max_output_tokens=value.max_output_tokens,
        max_usd=value.max_usd,
        max_wall=omp.Duration(value.max_wall) if value.max_wall is not None else None,
    )


def _spec(node: WorkflowNode, task: str, workflow_id: str) -> omp.agents.SubagentSpec:
    return omp.agents.SubagentSpec(
        task=task,
        name=node.name,
        agent=node.agent,
        system_prompt=node.system_prompt,
        model=node.model,
        thinking=node.thinking,
        allowed_devices=(
            frozenset(node.allowed_devices) if node.allowed_devices is not None else None
        ),
        disallowed_devices=frozenset(node.disallowed_devices),
        isolation=node.isolation,
        max_depth=node.max_depth,
        worktree=node.worktree,
        merge=(
            omp.agents.MergeMode.PATCH
            if node.worktree
            else omp.agents.MergeMode.NONE
        ),
        background=True,
        output_schema=node.output_schema,
        schema_mode=node.schema_mode,
        request_budget=node.request_budget,
        budget=_budget(node.budget),
        labels={"workflow": workflow_id, "node": node.name},
    )


def _strict_mapping(value: object, label: str) -> Mapping[str, object]:
    if not isinstance(value, Mapping):
        raise TypeError(f"{label} must be an object")
    if any(not isinstance(key, str) for key in value):
        raise TypeError(f"{label} keys must be strings")
    return value


def _configured_node(value: object) -> WorkflowNode:
    raw = dict(_strict_mapping(value, "workflow node"))
    budget_raw = raw.get("budget", {})
    raw["budget"] = WorkflowBudget(**dict(_strict_mapping(budget_raw, "node budget")))
    if thinking := raw.get("thinking"):
        raw["thinking"] = omp.agents.ThinkingLevel(str(thinking))
    if isolation := raw.get("isolation"):
        raw["isolation"] = omp.agents.Isolation(str(isolation))
    for field in ("allowed_devices", "disallowed_devices"):
        if field in raw and raw[field] is not None:
            value = raw[field]
            if not isinstance(value, list) or any(not isinstance(item, str) for item in value):
                raise TypeError(f"node {field} must be an array of strings")
            raw[field] = tuple(value)
    return WorkflowNode(**raw)


def _configured_edge(value: object) -> WorkflowEdge:
    return WorkflowEdge(**dict(_strict_mapping(value, "workflow edge")))


def _definition(args: WorkflowArgs, ctx: omp.Context) -> tuple[str, tuple[WorkflowNode, ...], tuple[WorkflowEdge, ...]]:
    if args.nodes:
        return args.name or "workflow", args.nodes, args.edges
    if args.edges:
        raise ValueError("edges cannot be supplied without nodes")

    encoded = ctx.settings.get("workflow", "")
    if not isinstance(encoded, str) or not encoded.strip():
        raise ValueError("supply nodes or configure the workflow JSON setting")
    raw = _strict_mapping(json.loads(encoded), "configured workflow")
    nodes_raw = raw.get("nodes")
    edges_raw = raw.get("edges", [])
    if not isinstance(nodes_raw, list) or not isinstance(edges_raw, list):
        raise TypeError("configured workflow nodes and edges must be arrays")
    name = args.name or raw.get("name", "workflow")
    if not isinstance(name, str):
        raise TypeError("workflow name must be a string")
    return (
        name,
        tuple(_configured_node(node) for node in nodes_raw),
        tuple(_configured_edge(edge) for edge in edges_raw),
    )


def _workflow_id(
    name: str, nodes: tuple[WorkflowNode, ...], edges: tuple[WorkflowEdge, ...]
) -> str:
    encoded = json.dumps(
        {
            "name": name,
            "nodes": [asdict(node) for node in nodes],
            "edges": [asdict(edge) for edge in edges],
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return f"{name}:{hashlib.sha256(encoded).hexdigest()[:16]}"


def _topological_waves(
    nodes: tuple[WorkflowNode, ...], edges: tuple[WorkflowEdge, ...]
) -> tuple[tuple[str, ...], ...]:
    if not nodes:
        raise ValueError("workflow must contain at least one node")
    order = [node.name for node in nodes]
    if any(not name.strip() for name in order):
        raise ValueError("workflow node names must not be empty")
    if len(set(order)) != len(order):
        raise ValueError("workflow node names must be unique")

    known = set(order)
    dependencies = {name: set[str]() for name in order}
    dependents = {name: set[str]() for name in order}
    for edge in edges:
        if edge.upstream not in known or edge.downstream not in known:
            raise ValueError(
                f"workflow edge {edge.upstream!r} -> {edge.downstream!r} names an unknown node"
            )
        if edge.upstream == edge.downstream:
            raise ValueError(f"workflow node {edge.upstream!r} cannot depend on itself")
        dependencies[edge.downstream].add(edge.upstream)
        dependents[edge.upstream].add(edge.downstream)

    remaining = {name: len(dependencies[name]) for name in order}
    ready = [name for name in order if remaining[name] == 0]
    waves: list[tuple[str, ...]] = []
    visited = 0
    while ready:
        wave = tuple(ready)
        waves.append(wave)
        visited += len(wave)
        released = set(wave)
        ready = []
        for name in order:
            if remaining[name] == 0 or name in released:
                continue
            remaining[name] -= len(dependencies[name] & released)
            if remaining[name] == 0:
                ready.append(name)
    if visited != len(nodes):
        raise ValueError("workflow edges contain a cycle")
    return tuple(waves)


def _dependencies(
    nodes: tuple[WorkflowNode, ...], edges: tuple[WorkflowEdge, ...]
) -> dict[str, tuple[str, ...]]:
    order = {node.name: index for index, node in enumerate(nodes)}
    deps: dict[str, list[str]] = {node.name: [] for node in nodes}
    for edge in edges:
        if edge.upstream not in deps[edge.downstream]:
            deps[edge.downstream].append(edge.upstream)
    return {
        name: tuple(sorted(values, key=order.__getitem__)) for name, values in deps.items()
    }


def _receipts(
    workflow_id: str,
) -> tuple[dict[str, WorkflowNodeSettled], dict[str, WorkflowNodeSkipped]]:
    settled: dict[str, WorkflowNodeSettled] = {}
    skipped: dict[str, WorkflowNodeSkipped] = {}
    for entry in omp.journal.entries(WorkflowNodeSettled):
        value = entry.value
        if isinstance(value, WorkflowNodeSettled) and value.workflow_id == workflow_id:
            settled[value.node] = value
    for entry in omp.journal.entries(WorkflowNodeSkipped):
        value = entry.value
        if isinstance(value, WorkflowNodeSkipped) and value.workflow_id == workflow_id:
            skipped[value.node] = value
    return settled, skipped


def _idempotency_key(workflow_id: str, node: str, outcome: str) -> str:
    value = f"{workflow_id}\0{node}\0{outcome}".encode()
    return f"workflow-{hashlib.sha256(value).hexdigest()}"


def _task(node: WorkflowNode, dependencies: tuple[str, ...], settled: Mapping[str, WorkflowNodeSettled]) -> str:
    if not dependencies:
        return node.task
    references = "\n".join(
        f"- {dependency}: {settled[dependency].output_url}"
        for dependency in dependencies
    )
    return (
        f"{node.task}\n\n"
        "Upstream outputs are durable references. Read them as needed; do not request inline copies:\n"
        f"{references}"
    )


async def _settle(
    workflow_id: str,
    workflow_name: str,
    node: WorkflowNode,
    handle: omp.agents.SubagentHandle,
) -> WorkflowNodeSettled:
    try:
        result = await handle.wait()
    except asyncio.CancelledError:
        raise
    except Exception as error:
        receipt = WorkflowNodeSettled(
            workflow_id=workflow_id,
            workflow=workflow_name,
            node=node.name,
            status="failed",
            run_id=handle.run_id,
            session_id=handle.session_id,
            output_url=str(handle.output_url),
            transcript_url=str(handle.transcript_url),
            detail=type(error).__name__,
        )
    else:
        receipt = WorkflowNodeSettled(
            workflow_id=workflow_id,
            workflow=workflow_name,
            node=node.name,
            status=result.status.value,
            run_id=result.run_id,
            session_id=result.session_id,
            output_url=str(result.output_url),
            transcript_url=str(result.transcript_url),
            detail=None if result.fault is None else type(result.fault).__name__,
        )
    omp.journal.append(
        receipt,
        idempotency_key=_idempotency_key(workflow_id, node.name, "settled"),
    )
    return receipt


async def _run(
    name: str,
    nodes: tuple[WorkflowNode, ...],
    edges: tuple[WorkflowEdge, ...],
) -> WorkflowResult:
    waves = _topological_waves(nodes, edges)
    dependencies = _dependencies(nodes, edges)
    by_name = {node.name: node for node in nodes}
    workflow_id = _workflow_id(name, nodes, edges)
    settled, skipped = _receipts(workflow_id)
    resumed = set(settled) | set(skipped)

    for wave in waves:
        ready: list[WorkflowNode] = []
        for node_name in wave:
            if node_name in settled or node_name in skipped:
                continue
            blocked_by = tuple(
                dependency
                for dependency in dependencies[node_name]
                if dependency in skipped
                or (
                    dependency in settled
                    and settled[dependency].status != omp.agents.RunStatus.COMPLETED.value
                )
            )
            if blocked_by:
                record = WorkflowNodeSkipped(
                    workflow_id=workflow_id,
                    workflow=name,
                    node=node_name,
                    blocked_by=blocked_by,
                )
                omp.journal.append(
                    record,
                    idempotency_key=_idempotency_key(workflow_id, node_name, "skipped"),
                )
                skipped[node_name] = record
            else:
                ready.append(by_name[node_name])

        if not ready:
            continue
        specs = tuple(
            _spec(
                node,
                _task(node, dependencies[node.name], settled),
                workflow_id,
            )
            for node in ready
        )
        handles = await omp.agents.spawn_all(specs)
        receipts = await asyncio.gather(
            *(
                _settle(workflow_id, name, node, handle)
                for node, handle in zip(ready, handles, strict=True)
            )
        )
        settled.update((receipt.node, receipt) for receipt in receipts)

    outcomes: list[WorkflowNodeOutcome] = []
    for node in nodes:
        if receipt := settled.get(node.name):
            outcomes.append(
                WorkflowNodeOutcome(
                    node=node.name,
                    status=receipt.status,
                    output_url=receipt.output_url,
                    resumed=node.name in resumed,
                )
            )
        else:
            record = skipped[node.name]
            outcomes.append(
                WorkflowNodeOutcome(
                    node=node.name,
                    status="skipped",
                    blocked_by=record.blocked_by,
                    resumed=node.name in resumed,
                )
            )
    return WorkflowResult(workflow_id, name, tuple(outcomes))


@omp.device("workflow", family="agents", rev=1, place="host")
async def workflow(args: WorkflowArgs, ctx: omp.Context) -> WorkflowResult:
    """Run or resume a declarative DAG in all-or-nothing topological waves."""

    name, nodes, edges = _definition(args, ctx)
    return await _run(name, nodes, edges)
