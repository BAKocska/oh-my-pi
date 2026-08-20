"""Hide optional transcript detail without changing stored calls or messages."""

from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import ui

_SCOPE = omp.StateScope.SESSION
_CORE_TOOLS = ("read", "bash", "grep", "glob", "edit", "write")


@omp.entry_kind("examples.calm-mode.toggle", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class CalmModeState:
    """Record whether calm transcript presentation is enabled for this session."""

    enabled: bool


_calm_enabled = True


def _state_value(record: object) -> bool | None:
    """Extract a valid toggle value from a host state record."""

    value = getattr(record, "value", None)
    return value.enabled if isinstance(value, CalmModeState) else None


def _tool_row(view: omp.View[object, object, object], ctx: ui.RenderCtx) -> ui.Tml | None:
    """Project a settled core call to one line only while calm presentation applies."""

    if not _calm_enabled or not ctx.collapsed or ctx.place is not ui.RenderPlace.TRANSCRIPT:
        return None
    if view.verdict is None:
        return ui.tml("<row><text fg=muted>{name}…</text></row>", name=view.identity.name)
    if isinstance(view.verdict, omp.Ok):
        return ui.tml(
            "<row>{icon} <text fg=muted>{name}</text></row>",
            icon=ui.icon("check", fg="ok"),
            name=view.identity.name,
        )
    if isinstance(view.verdict, omp.Faulted):
        return ui.tml(
            "<row>{icon} <text fg=muted>{name} failed</text></row>",
            icon=ui.icon("error", fg="err"),
            name=view.identity.name,
        )
    return None


# These exact revision registrations alter presentation only; dispatch and verdict storage
# remain owned by the built-in devices.
for _name in _CORE_TOOLS:
    omp.renderer(_name, rev=1)(_tool_row)


@omp.message_renderer("reasoning")
def render_reasoning(message: omp.MessageView, ctx: ui.RenderCtx) -> ui.Tml | None:
    """Suppress thinking only in the calm transcript view."""

    del message
    if _calm_enabled and ctx.place is ui.RenderPlace.TRANSCRIPT:
        return ui.Tml.raw("")
    return None


@omp.hook("extension_activate", phase=omp.HookPhase.OBSERVE)
async def activate_calm_mode(event: omp.ExtensionActivateEvent, ctx: omp.Context) -> None:
    """Restore the latest session toggle before folds are served."""

    del event, ctx
    global _calm_enabled
    record = await omp.state.latest(CalmModeState, scope=_SCOPE)
    stored = _state_value(record)
    _calm_enabled = True if stored is None else stored


@omp.command("calm", description="Toggle quiet transcript presentation")
async def toggle_calm(invocation: ui.Invocation, ctx: omp.Context) -> ui.Consumed:
    """Flip and record calm presentation for this session."""

    del invocation, ctx
    global _calm_enabled
    enabled = not _calm_enabled
    await omp.state.append(CalmModeState(enabled), scope=_SCOPE)
    _calm_enabled = enabled
    label = "enabled" if enabled else "disabled"
    return ui.Consumed(ui.text(f"Calm transcript presentation {label}."))
