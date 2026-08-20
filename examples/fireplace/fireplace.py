"""A retained fireplace whose motion belongs entirely to TML."""

from __future__ import annotations

import omp
from omp import ui

_MOUNT_KEY = "fireplace"
_ROOT_ID = "fireplace-root"
_smoke_enabled: bool | None = None


def _smoke_setting(ctx: omp.Context) -> bool:
    """Read the typed initial smoke preference."""

    value = ctx.settings.get("smoke", True)
    return value if isinstance(value, bool) else True


def _smoke_value(enabled: bool) -> str:
    """Encode the value consumed by the retained `when` expression."""

    return "on" if enabled else "off"


def _fireplace_tml(charset: ui.Charset, smoke: bool) -> ui.Tml:
    """Build one engine-animated fireplace for the active glyph tier."""

    fire = ui.icon("fire")
    smoke_icon = ui.icon("cloud")
    smoke_value = _smoke_value(smoke)
    if charset is ui.Charset.ASCII:
        return ui.tml(
            "<col id={root} value={smoke_value} align=center gap=0 noselect>"
            "<row id=fireplace-smoke when='fireplace-root=on' justify=center "
            "fg=muted anim=350ms ease=out>{smoke_icon}</row>"
            "<row justify=center fg='warn..err' angle=90 spin=1.2s "
            "anim=220ms ease=in-out>{open}{fire}{close}</row>"
            "</col>",
            root=_ROOT_ID,
            smoke_value=smoke_value,
            smoke_icon=smoke_icon,
            open=ui.text("("),
            fire=fire,
            close=ui.text(")"),
        )

    return ui.tml(
        "<col id={root} value={smoke_value} align=center gap=0 noselect>"
        "<row id=fireplace-smoke when='fireplace-root=on' justify=center "
        "fg=muted anim=350ms ease=out>{smoke_icon}</row>"
        "<row justify=center gap=1 fg='warn..err' angle=90 spin=900ms "
        "anim=220ms ease=in-out>{left}{middle}{right}</row>"
        "<text fg='warn..err' shimmer=1.4s anim=250ms ease=in-out>hearth</text>"
        "</col>",
        root=_ROOT_ID,
        smoke_value=smoke_value,
        smoke_icon=smoke_icon,
        left=fire,
        middle=fire,
        right=fire,
    )


async def _mount(ctx: omp.Context, smoke: bool) -> None:
    """Mount the fireplace after reading presentation content facts."""

    presentation = await ui.presentation()
    ui.mount(
        ui.Slot.FOOTER,
        _fireplace_tml(presentation.charset, smoke),
        ui.SlotOptions(order=90, max_height=3, collapse=ui.Collapse.TRUNCATE),
        key=_MOUNT_KEY,
    )


@omp.command(
    "fireplace",
    description="Toggle a cozy animated fireplace or its smoke",
    args=(ui.Arg("smoke", "Toggle smoke without hiding the fireplace"),),
    hint="[smoke]",
)
async def fireplace(inv: ui.Invocation, ctx: omp.Context) -> ui.Consumed:
    """Toggle the footer fireplace, or toggle its retained smoke prop."""

    global _smoke_enabled

    if _smoke_enabled is None:
        _smoke_enabled = _smoke_setting(ctx)

    if inv.argv == ("smoke",):
        _smoke_enabled = not _smoke_enabled
        try:
            handle = ui.handle(_MOUNT_KEY)
        except KeyError:
            await _mount(ctx, _smoke_enabled)
        else:
            handle.patch(_ROOT_ID, value=_smoke_value(_smoke_enabled))
        state = "on" if _smoke_enabled else "off"
        return ui.Consumed(ui.text(f"Fireplace smoke: {state}"))

    if inv.argv:
        return ui.Consumed(ui.text("Usage: /fireplace [smoke]"))

    try:
        ui.unmount(_MOUNT_KEY)
    except KeyError:
        await _mount(ctx, _smoke_enabled)
        return ui.Consumed(ui.text("Fireplace lit."))
    return ui.Consumed(ui.text("Fireplace extinguished."))
