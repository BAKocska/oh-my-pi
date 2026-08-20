"""Declarative hover and lift treatment for one extension-owned tool card."""

from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import Context, Faulted, Ok, Payload, View


@dataclass(frozen=True, slots=True)
class CardArgs:
    """Text used to demonstrate an enriched transcript result card."""

    title: str
    summary: str
    detail: str


@dataclass(frozen=True, slots=True)
class CardPayload(Payload):
    """Durable content projected into the compact and detailed card views."""

    title: str
    summary: str
    detail: str


@dataclass(frozen=True, slots=True)
class CardFault(omp.Fault):
    """A rejected card whose required text was empty."""

    detail: str


@omp.device("hover_card", family="hover-card", rev=1)
async def hover_card(args: CardArgs, ctx: Context) -> CardPayload | CardFault:
    """Return user-safe content for the declaratively interactive result card."""

    del ctx
    if not args.title.strip() or not args.summary.strip() or not args.detail.strip():
        return CardFault("title, summary, and detail must all be non-empty")
    return CardPayload(args.title.strip(), args.summary.strip(), args.detail.strip())


def detail_overlay(payload: CardPayload) -> omp.ui.Tml:
    """Build the modal detail document for one card payload."""

    return omp.ui.tml(
        "<box title='Tool result detail' border=round pad=1>"
        "<text bold>{title}</text>"
        "<approval><text>{detail}</text></approval>"
        "<row justify=end><button id=close label=Close cancel/></row>"
        "</box>",
        title=payload.title,
        detail=payload.detail,
    )


async def open_detail(payload: CardPayload, ctx: Context) -> None:
    """Open and own the card's modal detail overlay until it is dismissed."""

    if not ctx.has_ui:
        return
    async with await omp.ui.overlay(
        detail_overlay(payload),
        omp.ui.OverlayOptions(width=omp.ui.Pct(70), max_height=omp.ui.Pct(70)),
    ) as overlay:
        await overlay.wait()


@omp.renderer("hover_card", family="hover-card", rev=1)
def render_hover_card(
    view: View[object, CardPayload, CardFault], ctx: omp.ui.RenderCtx
) -> omp.ui.Tml | None:
    """Render a focusable card whose hover gradient and lift animate declaratively."""

    if view.verdict is None:
        return omp.ui.tml("<row>{icon}<text dim> preparing card</text></row>", icon=omp.ui.icon("info"))
    match view.verdict:
        case Ok(payload):
            border = "accent" if ctx.focused else "muted"
            detail = "" if ctx.collapsed else payload.detail
            return omp.ui.tml(
                "<box id=hover-card-detail focus border=round bc={border} "
                "hover='accent..info' lift anim=180ms ease=in-out pad=1>"
                "<row gap=1>{icon}<text bold>{title}</text></row>"
                "<text>{summary}</text><text dim>{detail}</text>"
                "</box>",
                border=border,
                icon=omp.ui.icon("info"),
                title=payload.title,
                summary=payload.summary,
                detail=detail,
            )
        case Faulted():
            return None
