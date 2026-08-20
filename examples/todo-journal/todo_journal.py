from __future__ import annotations

from dataclasses import dataclass, replace
from typing import Literal

import omp
from omp import Context, Ok, Payload, View
from omp import entry_kind, journal


_TodoOp = Literal["add", "done", "drop"]


@entry_kind("examples.todo-journal.item", rev="v.1", spill=False)
@dataclass(frozen=True, slots=True)
class TodoItem:
    """One durable mutation in the session's todo history."""

    op: _TodoOp
    task_id: int | None = None
    text: str | None = None


@dataclass(frozen=True, slots=True)
class TodoChange:
    """One requested add, done, or drop operation."""

    op: _TodoOp
    task_id: int | None = None
    text: str | None = None


@dataclass(frozen=True, slots=True)
class TodoArgs:
    """Arguments for one idempotent todo mutation request."""

    changes: list[TodoChange]
    idempotency_key: str


@dataclass(frozen=True, slots=True)
class Todo:
    """One item in the folded current checklist."""

    task_id: int
    text: str
    done: bool = False


@dataclass(frozen=True, slots=True)
class TodoPayload(Payload):
    """The current checklist after applying a request."""

    items: list[Todo]


@dataclass(frozen=True, slots=True)
class TodoFault(omp.Fault):
    """A rejected todo mutation with a model-readable reason."""

    detail: str


def _fold_current() -> list[Todo]:
    tasks: dict[int, Todo] = {}
    for entry in journal.entries(TodoItem):
        mutation = entry.value
        if not isinstance(mutation, TodoItem):
            raise RuntimeError(f"todo entry {entry.id} is not decodable")

        if mutation.op == "add":
            tasks[entry.id.index] = Todo(entry.id.index, mutation.text or "")
        elif mutation.op == "done" and mutation.task_id in tasks:
            task = tasks[mutation.task_id]
            tasks[mutation.task_id] = replace(task, done=True)
        elif mutation.op == "drop" and mutation.task_id is not None:
            tasks.pop(mutation.task_id, None)

    return list(tasks.values())


def _validate(changes: list[TodoChange]) -> str | None:
    if not changes:
        return "changes must contain at least one operation"
    for change in changes:
        if change.op == "add":
            if not change.text or not change.text.strip():
                return "add requires non-empty text"
            if change.task_id is not None:
                return "add does not accept task_id; the journal index becomes the id"
        elif change.task_id is None:
            return f"{change.op} requires task_id"
        elif change.text is not None:
            return f"{change.op} does not accept text"
    return None


def _entries(changes: list[TodoChange]) -> list[TodoItem]:
    return [TodoItem(change.op, change.task_id, change.text) for change in changes]


@omp.device("todo", family="v", rev=1, place="host")
async def todo(args: TodoArgs, ctx: Context) -> TodoPayload | TodoFault:
    """Add, complete, or drop todo items, then return the journal-folded list."""

    del ctx
    if not args.idempotency_key.strip():
        return TodoFault("idempotency_key must not be empty")
    if error := _validate(args.changes):
        return TodoFault(error)

    entries = _entries(args.changes)
    if len(entries) == 1:
        journal.append(entries[0], idempotency_key=args.idempotency_key)
    else:
        await journal.append_many(entries, idempotency_key=args.idempotency_key)
    return TodoPayload(_fold_current())


@omp.renderer("todo", family="v", rev=1)
def render_todo(
    view: View[object, TodoPayload, TodoFault], ctx: omp.ui.RenderCtx
) -> omp.ui.Tml | None:
    """Render the settled todo fold as a Markdown checklist."""

    if view.verdict is None:
        return omp.ui.tml("<row>{icon} updating todo list</row>", icon=omp.ui.icon("list"))
    match view.verdict:
        case Ok(payload):
            shown = payload.items[:5] if ctx.collapsed else payload.items
            if not shown:
                return omp.ui.md("Todo list is empty.")
            lines = [
                f"- [{'x' if item.done else ' '}] #{item.task_id} {item.text}"
                for item in shown
            ]
            if len(shown) < len(payload.items):
                lines.append(f"- … {len(payload.items) - len(shown)} more")
            return omp.ui.md("\n".join(lines))
        case _:
            return None
