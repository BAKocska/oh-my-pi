"""Declarative startup chrome with native session selection and lifecycle effects."""

from __future__ import annotations

from collections.abc import Mapping, Sequence

import omp
from omp import sessions, ui

_HEADER_KEY = "welcome-chrome"
_DEFAULT_TIPS = (
    "Use /help to discover commands without leaving the composer.",
    "Resume a recent session to keep its durable journal and context.",
    "Named icons and theme tokens adapt to the active terminal.",
)
_RECENT_LIMIT = 6
_tip_cursor = 0


def _tips(settings: Mapping[str, object]) -> tuple[str, ...]:
    """Return non-empty configured tips, or the built-in rotation."""
    configured = settings.get("tips", _DEFAULT_TIPS)
    if not isinstance(configured, Sequence) or isinstance(
        configured, (str, bytes, bytearray)
    ):
        return _DEFAULT_TIPS
    tips = tuple(str(value).strip() for value in configured if str(value).strip())
    return tips or _DEFAULT_TIPS


def _next_tip(settings: Mapping[str, object]) -> str:
    """Advance tips only when lifecycle state changes, never from a timer."""
    global _tip_cursor
    tips = _tips(settings)
    tip = tips[_tip_cursor % len(tips)]
    _tip_cursor += 1
    return tip


def _header_tml(tip: str, *, active: bool) -> ui.Tml:
    """Build retained header markup whose motion is owned by the TUI clock."""
    activity = (
        ui.tml("<spinner fg=accent>Agent working</spinner>")
        if active
        else ui.tml("<row gap=1>{icon}<text fg=muted>Ready</text></row>", icon=ui.icon("play"))
    )
    return ui.tml(
        "<box border=round bc='accent..info' anim=220ms ease=in-out pad='0 1'>"
        "<col gap=1>"
        "<row gap=1 fg='accent..info' angle=0 spin=3s anim=220ms ease=in-out>"
        "{mark}<text bold shimmer=2.4s reveal=350ms>Welcome to omp</text>"
        "<spacer/>{activity}"
        "</row>"
        "<row gap=1>{tip_icon}<text id=welcome-tip fg=muted reveal=300ms "
        "anim=180ms ease=out>{tip}</text></row>"
        "</col>"
        "</box>",
        mark=ui.icon("sparkles"),
        activity=activity,
        tip_icon=ui.icon("lightbulb"),
        tip=tip,
    )


def _mount_header(ctx: omp.Context, *, active: bool) -> ui.SlotHandle:
    """Mount or replace the user-layout-arbitrated startup header."""
    return ui.mount(
        ui.Slot.HEADER,
        _header_tml(_next_tip(ctx.settings), active=active),
        ui.SlotOptions(
            order=40,
            min_width=32,
            max_height=4,
            collapse=ui.Collapse.SHRINK,
        ),
        key=_HEADER_KEY,
    )


def _unmount_header() -> None:
    """Unmount the header when this session releases its presentation."""
    try:
        ui.unmount(_HEADER_KEY)
    except KeyError:
        pass


def _recent_items(
    rows: Sequence[sessions.SessionInfo], current_session: str
) -> tuple[ui.SelectItem, ...]:
    """Project recent interactive sessions into native selectable rows."""
    items: list[ui.SelectItem] = []
    for row in rows:
        if row.id == current_session:
            continue
        title = row.title.strip() if row.title and row.title.strip() else "Untitled session"
        location = row.project or str(row.cwd)
        remote = " | remote" if row.remote else ""
        items.append(
            ui.SelectItem(
                value=row.id,
                label=title,
                desc=f"{row.turns} turns | {row.status.value} | {location}{remote}",
                preview=ui.tml(
                    "<col gap=1><row gap=1>{icon}<text bold>{title}</text></row>"
                    "<text fg=muted>{entries} journal entries</text></col>",
                    icon=ui.icon("history"),
                    title=title,
                    entries=str(row.entries),
                ),
            )
        )
    return tuple(items)


async def _offer_recent_sessions(ctx: omp.Context) -> sessions.SessionInfo | None:
    """Offer recent rows through the native picker and resume the chosen id."""
    rows = await sessions.list(sessions.SessionFilter(limit=_RECENT_LIMIT + 1))
    items = _recent_items(rows, ctx.session)[:_RECENT_LIMIT]
    if not items:
        return None
    outcome = await ui.select(
        "Recent sessions",
        items,
        options=ui.DialogOptions(
            help="Enter resumes the selected session; Escape starts here.",
        ),
    )
    if outcome.cancelled or outcome.value is None:
        return None
    return await sessions.resume(outcome.value)


@omp.hook("session_start", phase=omp.HookPhase.OBSERVE)
async def show_welcome(event: omp.SessionStartEvent, ctx: omp.Context) -> None:
    """Mount startup chrome and offer native recent-session selection."""
    del event
    if not ctx.has_ui:
        return
    _mount_header(ctx, active=False)
    await _offer_recent_sessions(ctx)


@omp.hook("agent_start", phase=omp.HookPhase.OBSERVE)
async def show_activity(event: omp.AgentStartEvent, ctx: omp.Context) -> None:
    """Show retained activity chrome and set a sanitized terminal title."""
    del event
    if not ctx.has_ui:
        return
    _mount_header(ctx, active=True)
    ui.set_title("omp | working")


@omp.hook("agent_end", phase=omp.HookPhase.OBSERVE)
async def settle_activity(event: omp.AgentEndEvent, ctx: omp.Context) -> None:
    """Restore idle chrome and omp's generated terminal title."""
    del event
    if ctx.has_ui:
        _mount_header(ctx, active=False)
    ui.set_title(None)


@omp.hook("session_shutdown", phase=omp.HookPhase.OBSERVE)
async def remove_welcome(event: omp.SessionShutdownEvent, ctx: omp.Context) -> None:
    """Release the header and restore the title during bounded shutdown."""
    del event, ctx
    _unmount_header()
    ui.set_title(None)
