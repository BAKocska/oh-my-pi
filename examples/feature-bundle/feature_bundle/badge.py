"""Optional retained status badge for the feature-bundle example."""

from __future__ import annotations

import omp
from omp import ui


@omp.hook("extension_activate")
async def paint_badge(payload: object, ctx: omp.Context) -> None:
    """Publish one small statusline segment when the bundle activates."""

    del payload, ctx
    ui.set_status(
        "feature-bundle",
        ui.tml("<segment fg=accent>{label}</segment>", label=ui.text("bundle")),
        order=40,
        side=ui.Slot.STATUS_RIGHT,
    )
