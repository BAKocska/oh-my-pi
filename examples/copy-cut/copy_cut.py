"""Clipboard cut shortcut backed by bounded session state."""

from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import ui


_MAX_CUT_BYTES = 65_536


@omp.entry_kind(
    "examples.copy-cut.last-cut", rev="v.1", display=False, spill=False
)
@dataclass(frozen=True, slots=True)
class LastCut:
    """Store the most recent bounded composer cut for this session."""

    text: str


def _write_clipboard(text: str) -> None:
    """Send text through the client-owned clipboard effect."""

    # GAP: the frozen UI layer has no clipboard/OSC 52 effect yet.
    ui.set_clipboard(text)  # type: ignore[attr-defined]


@ui.shortcut(
    "alt+shift+x",
    action_id="copy-cut.cut",
    description="Cut composer text to the clipboard",
)
async def cut_composer(action: ui.Action, ctx: omp.Context) -> None:
    """Copy bounded composer text, remember it, and clear the composer."""

    text = await ui.editor_text()
    if not text:
        return
    if len(text.encode("utf-8")) > _MAX_CUT_BYTES:
        ui.notify(
            "Composer text exceeds the 64 KiB cut limit.",
            level=ui.Level.WARN,
        )
        return

    _write_clipboard(text)
    await omp.state.append(LastCut(text), scope=omp.StateScope.SESSION)
    ui.set_editor_text("")


@omp.command(
    "paste-cut",
    description="Restore the most recent composer cut from this session",
)
async def paste_cut(invocation: ui.Invocation, ctx: omp.Context) -> ui.Consumed | None:
    """Restore the latest session-scoped cut to the composer."""

    record = await omp.state.latest(LastCut, scope=omp.StateScope.SESSION)
    if record is None or not isinstance(record.value, LastCut):
        ui.notify("No composer cut is stored in this session.", level=ui.Level.WARN)
        return None

    ui.set_editor_text(record.value.text)
    return ui.Consumed()
