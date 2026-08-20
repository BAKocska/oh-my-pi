"""Indexed session picker with explicit, approval-shaped destructive actions."""

from __future__ import annotations

from dataclasses import dataclass, replace
from datetime import datetime, timezone
from typing import Sequence

import omp
from omp import ui


_MAX_SESSIONS = 200


@dataclass(frozen=True, slots=True)
class _SessionRow:
    id: str
    title: str
    updated_ms: int
    turns: int
    tokens: int
    cost_usd: float


def _rows(infos: Sequence[omp.sessions.SessionInfo], query: str = "") -> tuple[_SessionRow, ...]:
    """Project indexed rows in recency order and filter titles locally."""

    needle = query.strip().casefold()
    projected = (
        _SessionRow(
            id=info.id,
            title=info.title or "Untitled session",
            updated_ms=info.updated_ms,
            turns=info.turns,
            tokens=info.usage.total,
            cost_usd=info.cost.usd,
        )
        for info in infos
        if not needle or needle in (info.title or "").casefold()
    )
    return tuple(sorted(projected, key=lambda row: row.updated_ms, reverse=True))


def _preview_rename(
    rows: Sequence[_SessionRow], session_id: str, title: str
) -> tuple[_SessionRow, ...]:
    """Apply a title to an immutable picker projection without claiming persistence."""

    clean = title.strip()
    if not clean:
        raise ValueError("session title must not be empty")
    found = False
    renamed: list[_SessionRow] = []
    for row in rows:
        if row.id == session_id:
            row = replace(row, title=clean)
            found = True
        renamed.append(row)
    if not found:
        raise KeyError(session_id)
    return tuple(renamed)


def _delete_decision(session_id: str, title: str | None = None) -> omp.RequireApproval:
    """Describe irreversible deletion as a Core-owned durable approval ticket."""

    subject = f"the session titled {title!r}" if title else f"session {session_id!r}"
    return omp.RequireApproval(
        omp.ApprovalSpec(
            title="Delete session",
            body=f"Permanently delete {subject}? This cannot be undone.",
            subject=session_id,
            kind=omp.ApprovalKind.DEVICE,
            scopes=(omp.PolicyScope.ONCE,),
            require_human=True,
            pattern=session_id,
        )
    )


def _stamp(updated_ms: int) -> str:
    return datetime.fromtimestamp(updated_ms / 1000, tz=timezone.utc).strftime("%Y-%m-%d %H:%MZ")


def _picker(rows: Sequence[_SessionRow]) -> ui.Tml:
    options = tuple(
        ui.tml(
            "<option value='{value}'>"
            "<td min=24 grow=1 truncate=end>{title}</td>"
            "<td><text fg=muted>{updated}</text></td>"
            "<td><text>{turns}</text></td>"
            "<td><text>{tokens}</text></td>"
            "<td><text>${cost}</text></td>"
            "</option>",
            value=f"s{index}",
            title=row.title,
            updated=_stamp(row.updated_ms),
            turns=str(row.turns),
            tokens=f"{row.tokens:,}",
            cost=f"{row.cost_usd:.4f}",
        )
        for index, row in enumerate(rows)
    )
    return ui.tml(
        "<box title='Session browser' border=round pad='1 2'>"
        "<col gap=1>"
        "<text fg=muted>Type in the session list to filter titles locally.</text>"
        "<table gap=2><tr>"
        "<td><text bold>Title</text></td><td><text bold>Updated</text></td>"
        "<td><text bold>Turns</text></td><td><text bold>Tokens</text></td>"
        "<td><text bold>Cost</text></td>"
        "</tr></table>"
        "<select id=session filter h=14>{options}</select>"
        "<radio id=action options='resume rename delete' value=resume/>"
        "<input id=title placeholder='New title (rename only)'/>"
        "<text fg=warn>Delete is staged as an explicit command and requires Core approval.</text>"
        "<row justify=end gap=1>"
        "<button label=Cancel cancel/><button label=Continue submit accent/>"
        "</row>"
        "</col></box>",
        options=options,
    )


def _selected(values: dict[str, object], rows: Sequence[_SessionRow]) -> tuple[_SessionRow, str, str]:
    selected = values.get("session")
    if not isinstance(selected, str) or not selected.startswith("s"):
        raise ValueError("select a session")
    try:
        row = rows[int(selected[1:])]
    except (ValueError, IndexError) as error:
        raise ValueError("select a session") from error
    action = values.get("action")
    if action not in {"resume", "rename", "delete"}:
        raise ValueError("select an action")
    title = values.get("title")
    return row, str(action), title if isinstance(title, str) else ""


def _staged_command(row: _SessionRow, action: str, title: str) -> str:
    if action == "rename":
        clean = title.strip()
        if not clean:
            raise ValueError("enter a new title")
        _preview_rename((row,), row.id, clean)
        return f"/sessions rename {row.id} {clean}"
    return f"/sessions {action} {row.id}"


@omp.hook(
    "command_invoke",
    phase=omp.HookPhase.APPROVAL,
    on_failure=omp.OnFailure.DENY,
    when=omp.When(name=frozenset({"sessions"})),
)
async def approve_session_delete(
    payload: omp.CommandInvokeEvent, ctx: omp.Context
) -> omp.HookDecision:
    """Require one human approval ticket for every explicit delete command."""

    del ctx
    if len(payload.argv) < 2 or payload.argv[0] != "delete":
        return omp.Defer()
    return _delete_decision(payload.argv[1])


@omp.command(
    "sessions",
    description="Browse indexed sessions and stage a management action",
    args=(
        ui.Arg("resume", "Resume a session", usage="<session-id>"),
        ui.Arg("rename", "Rename a session", usage="<session-id> <title>"),
        ui.Arg("delete", "Delete only after durable approval", usage="<session-id>"),
    ),
    hint="resume | rename | delete",
)
async def sessions(
    inv: ui.Invocation, ctx: omp.Context
) -> ui.Consumed | ui.Prompt | None:
    """Open the indexed picker or handle its explicit staged command."""

    if inv.argv:
        action = inv.argv[0]
        if action in {"resume", "rename", "delete"}:
            return ui.Consumed(
                ui.tml(
                    "<callout fg=warn>Action unavailable: the frozen Python layer has no "
                    "session {action} operation. No session was changed.</callout>",
                    action=action,
                )
            )
        return ui.Consumed(ui.text("Usage: /sessions [resume|rename|delete] ..."))

    if not ctx.has_ui:
        return ui.Consumed(ui.text("The session browser requires an interactive UI."))

    rows = _rows(await omp.sessions.list(omp.sessions.SessionFilter(limit=_MAX_SESSIONS)))
    if not rows:
        return ui.Consumed(ui.text("No indexed interactive sessions."))

    async with await ui.overlay(
        _picker(rows), ui.OverlayOptions(width=ui.Pct(90), max_height=ui.Pct(85))
    ) as overlay:
        outcome = await overlay.wait()
        if outcome.cancelled:
            return None
        values = await overlay.values()

    try:
        row, action, title = _selected(values, rows)
        staged = _staged_command(row, action, title)
    except ValueError as error:
        return ui.Consumed(ui.text(str(error)))

    return ui.Prompt(staged, submit=False)
