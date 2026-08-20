from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Literal

import omp


_TaskOperation = Literal["add", "link"]
_rule_snapshot: tuple["Rule", ...] = ()
_INDEX_NAME = "adjacency-v1.json"


@omp.entry_kind("examples.task-graph.task", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class Task:
    """Record one durable task-node or task-edge mutation."""

    operation: _TaskOperation
    task_id: str
    title: str | None = None
    active: bool = True
    related_id: str | None = None


@omp.entry_kind("examples.task-graph.rule", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class Rule:
    """Record the latest durable state of one graph rule."""

    rule_id: str
    text: str
    active: bool = True


@dataclass(frozen=True, slots=True)
class GraphArgs:
    """Request summary counts for the durable graph."""


@dataclass(frozen=True, slots=True)
class AddTaskArgs:
    """Add or replace one durable task node."""

    task_id: str
    title: str
    active: bool = True


@dataclass(frozen=True, slots=True)
class LinkTaskArgs:
    """Link a task to one prerequisite task."""

    task_id: str
    depends_on: str


@dataclass(frozen=True, slots=True)
class NextTaskArgs:
    """Bound the number of ready active tasks returned."""

    limit: int = 10


@dataclass(frozen=True, slots=True)
class RuleListArgs:
    """Choose whether to list inactive rules as well."""

    active_only: bool = True


@dataclass(frozen=True, slots=True)
class TaskView:
    """Project one task without exposing index implementation details."""

    task_id: str
    title: str
    active: bool
    depends_on: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class RuleView:
    """Project one durable rule in deterministic identifier order."""

    rule_id: str
    text: str
    active: bool


@dataclass(frozen=True, slots=True)
class GraphStatus:
    """Summarize task nodes, task edges, and active rules."""

    tasks: int
    links: int
    active_rules: int
    watermark: str | None


@dataclass(frozen=True, slots=True)
class TaskReceipt:
    """Confirm one durable task-node mutation."""

    task_id: str
    active: bool
    watermark: str


@dataclass(frozen=True, slots=True)
class LinkReceipt:
    """Confirm one durable prerequisite edge."""

    task_id: str
    depends_on: str
    watermark: str


@dataclass(frozen=True, slots=True)
class NextTasks:
    """Return ready tasks from the rebuilt adjacency index."""

    tasks: tuple[TaskView, ...]
    watermark: str | None


@dataclass(frozen=True, slots=True)
class RuleList:
    """Return durable rule snapshots in deterministic order."""

    rules: tuple[RuleView, ...]


@dataclass(slots=True)
class _GraphIndex:
    tasks: dict[str, TaskView]
    outgoing: dict[str, set[str]]
    watermark: str | None


def _clean_id(value: str, label: str) -> str:
    cleaned = value.strip()
    if not cleaned or any(character.isspace() for character in cleaned):
        raise ValueError(f"{label} must be a non-empty identifier without whitespace")
    return cleaned


def _replay_tasks(events: tuple[Task, ...], watermark: str | None = None) -> _GraphIndex:
    nodes: dict[str, tuple[str, bool]] = {}
    outgoing: dict[str, set[str]] = {}
    for event in events:
        if event.operation == "add" and event.title is not None:
            nodes[event.task_id] = (event.title, event.active)
            outgoing.setdefault(event.task_id, set())
        elif event.operation == "link" and event.related_id is not None:
            outgoing.setdefault(event.related_id, set()).add(event.task_id)

    prerequisites: dict[str, set[str]] = {task_id: set() for task_id in nodes}
    for dependency, dependents in outgoing.items():
        for dependent in dependents:
            if dependency in nodes and dependent in nodes:
                prerequisites[dependent].add(dependency)

    tasks = {
        task_id: TaskView(task_id, title, active, tuple(sorted(prerequisites[task_id])))
        for task_id, (title, active) in sorted(nodes.items())
    }
    clean_outgoing = {
        task_id: {dependent for dependent in outgoing.get(task_id, ()) if dependent in tasks}
        for task_id in tasks
    }
    return _GraphIndex(tasks, clean_outgoing, watermark)


def _replay_rules(events: tuple[Rule, ...]) -> tuple[Rule, ...]:
    latest: dict[str, Rule] = {}
    for event in events:
        latest[event.rule_id] = event
    return tuple(latest[key] for key in sorted(latest))


def _encode_index(index: _GraphIndex) -> str:
    body = {
        "watermark": index.watermark,
        "tasks": [
            {"id": task.task_id, "title": task.title, "active": task.active}
            for task in index.tasks.values()
        ],
        "outgoing": {
            task_id: sorted(index.outgoing[task_id]) for task_id in sorted(index.outgoing)
        },
    }
    return json.dumps(body, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def _decode_index(raw: str) -> _GraphIndex:
    body = json.loads(raw)
    events: list[Task] = []
    for row in body["tasks"]:
        events.append(Task("add", row["id"], row["title"], row["active"]))
    for dependency, dependents in body["outgoing"].items():
        for dependent in dependents:
            events.append(Task("link", dependent, related_id=dependency))
    watermark = body.get("watermark")
    if watermark is not None and not isinstance(watermark, str):
        raise TypeError("index watermark must be a string or null")
    return _replay_tasks(tuple(events), watermark)


def _would_cycle(index: _GraphIndex, task_id: str, dependency: str) -> bool:
    pending = [task_id]
    seen: set[str] = set()
    while pending:
        current = pending.pop()
        if current == dependency:
            return True
        if current in seen:
            continue
        seen.add(current)
        pending.extend(index.outgoing.get(current, ()))
    return False


def _task_rows() -> tuple[tuple[Task, ...], str | None]:
    rows = tuple(omp.journal.entries(Task))
    watermark = str(rows[-1].id) if rows else None
    return tuple(row.value for row in rows), watermark


def _rule_rows() -> tuple[Rule, ...]:
    return tuple(row.value for row in omp.journal.entries(Rule))


async def _read_index() -> _GraphIndex | None:
    state = await omp.state_dir()
    try:
        async with await omp.env.docs.open(state.join(_INDEX_NAME)) as document:
            return _decode_index(await document.read())
    except (omp.env.NotFound, json.JSONDecodeError, KeyError, TypeError, ValueError):
        return None


async def _write_index(index: _GraphIndex) -> None:
    state = await omp.state_dir()
    async with await omp.env.docs.open(state.join(_INDEX_NAME), create=True) as document:
        await document.write(_encode_index(index))


async def _graph_index(*, force: bool = False) -> _GraphIndex:
    events, watermark = _task_rows()
    if not force:
        cached = await _read_index()
        if cached is not None and cached.watermark == watermark:
            return cached
    rebuilt = _replay_tasks(events, watermark)
    await _write_index(rebuilt)
    return rebuilt


def _refresh_rules() -> tuple[Rule, ...]:
    global _rule_snapshot
    _rule_snapshot = _replay_rules(_rule_rows())
    return _rule_snapshot


def _render_rules(rules: tuple[Rule, ...], budget: int) -> str | None:
    active = tuple(rule for rule in rules if rule.active)
    if not active or budget <= 0:
        return None
    opening = "<active-rules>\n"
    closing = "</active-rules>"
    if len((opening + closing).encode()) > budget:
        return None
    parts = [opening]
    used = len((opening + closing).encode())
    for rule in sorted(active, key=lambda item: item.rule_id):
        text = " ".join(rule.text.split())
        line = f"- {rule.rule_id}: {text}\n"
        encoded = len(line.encode())
        if used + encoded > budget:
            continue
        parts.append(line)
        used += encoded
    parts.append(closing)
    return "".join(parts)


@omp.prompt_slot("rules", priority=100, cls=omp.SlotClass.STABLE)
def active_rules_prompt(ctx: omp.PromptContext) -> str | None:
    """Inject a bounded, deterministic snapshot containing only active rules."""

    return _render_rules(_rule_snapshot, ctx.budget_bytes)


@omp.device(
    "graph",
    family="task_graph",
    rev=1,
    place="host",
    summary="Query the durable task-and-rule graph.",
)
async def graph(args: GraphArgs, ctx: omp.Context) -> GraphStatus:
    """Summarize the durable graph through its rebuildable adjacency index."""

    del args, ctx
    index = await _graph_index()
    rules = _refresh_rules()
    return GraphStatus(
        tasks=len(index.tasks),
        links=sum(len(dependents) for dependents in index.outgoing.values()),
        active_rules=sum(rule.active for rule in rules),
        watermark=index.watermark,
    )


@graph.subtool("task/add")
async def add_task(args: AddTaskArgs, ctx: omp.Context) -> TaskReceipt:
    """Add or update one durable task node."""

    task_id = _clean_id(args.task_id, "task_id")
    title = args.title.strip()
    if not title:
        raise ValueError("title must not be empty")
    entry_id = omp.journal.append(
        Task("add", task_id, title, args.active),
        idempotency_key=f"task-add:{ctx.invocation}",
    )
    index = await _graph_index(force=True)
    return TaskReceipt(task_id, args.active, str(entry_id))


@graph.subtool("task/link")
async def link_task(args: LinkTaskArgs, ctx: omp.Context) -> LinkReceipt:
    """Link a task to one prerequisite without creating a cycle."""

    task_id = _clean_id(args.task_id, "task_id")
    dependency = _clean_id(args.depends_on, "depends_on")
    index = await _graph_index()
    if task_id not in index.tasks or dependency not in index.tasks:
        raise ValueError("task/link requires both task nodes to exist")
    if task_id == dependency or _would_cycle(index, task_id, dependency):
        raise ValueError("task/link would create a cycle")
    entry_id = omp.journal.append(
        Task("link", task_id, related_id=dependency),
        idempotency_key=f"task-link:{ctx.invocation}",
    )
    await _graph_index(force=True)
    return LinkReceipt(task_id, dependency, str(entry_id))


@graph.subtool("task/next")
async def next_task(args: NextTaskArgs, ctx: omp.Context) -> NextTasks:
    """List ready active tasks from the adjacency index."""

    del ctx
    if not isinstance(args.limit, int) or isinstance(args.limit, bool) or not 1 <= args.limit <= 100:
        raise ValueError("limit must be an integer from 1 through 100")
    index = await _graph_index()
    ready = tuple(
        task
        for task in index.tasks.values()
        if task.active
        and all(
            dependency in index.tasks and not index.tasks[dependency].active
            for dependency in task.depends_on
        )
    )
    return NextTasks(ready[: args.limit], index.watermark)


@graph.subtool("rule/list")
async def list_rules(args: RuleListArgs, ctx: omp.Context) -> RuleList:
    """List deterministic durable rule snapshots."""

    del ctx
    rules = _refresh_rules()
    return RuleList(
        tuple(
            RuleView(rule.rule_id, rule.text, rule.active)
            for rule in rules
            if rule.active or not args.active_only
        )
    )


@omp.hook("extension_activate", phase=omp.HookPhase.OBSERVE)
async def rebuild_graph(event: omp.ExtensionActivateEvent, ctx: omp.Context) -> None:
    """Rebuild derived graph state and prime rules before the first prompt."""

    del event, ctx
    _refresh_rules()
    await _graph_index(force=True)
