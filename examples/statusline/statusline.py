from __future__ import annotations

from hashlib import blake2s

from omp import hook
from omp import ui
from omp.ui import StatusFacts

_last_state_hash: bytes | None = None


def _state_hash(facts: StatusFacts, cwd: object, total_tokens: int) -> bytes:
    """Hash only the facts that affect this extension's retained chrome."""
    values = (
        facts.model,
        facts.context_tokens,
        facts.context_window,
        facts.cost_usd,
        cwd,
        total_tokens,
    )
    return blake2s(
        b"\0".join(str(value).encode("utf-8") for value in values),
        digest_size=16,
    ).digest()


def _paint(ctx: omp.Context, facts: StatusFacts, total_tokens: int) -> None:
    """Emit one keyed state update when the rendered facts have changed."""
    global _last_state_hash

    state_hash = _state_hash(facts, ctx.cwd, total_tokens)
    if state_hash == _last_state_hash:
        return

    ui.set_status(
        "model",
        ui.tml(
            "<segment fg=accent>{icon}{model}</segment>",
            icon=ui.icon("robot"),
            model=ui.text(facts.model),
        ),
        order=10,
        side=ui.Slot.STATUS_LEFT,
    )

    context_window = max(facts.context_window, 1)
    context_percent = 100 * facts.context_tokens // context_window
    tone = ui.Token.ERR if context_percent > 90 else ui.Token.WARN if context_percent > 70 else ui.Token.MUTED
    ui.set_status(
        "context",
        ui.tml(
            "<segment fg={tone}>{percent}</segment>",
            tone=tone,
            percent=ui.text(f"{context_percent}% ctx"),
        ),
        order=20,
        side=ui.Slot.STATUS_LEFT,
    )

    ui.set_status(
        "tokens",
        ui.tml(
            "<segment fg=secondary>{icon}{tokens}</segment>",
            icon=ui.icon("hash"),
            tokens=ui.text(f"{total_tokens:,} tok"),
        ),
        order=30,
        side=ui.Slot.STATUS_RIGHT,
    )

    ui.mount(
        ui.Slot.FOOTER,
        ui.tml(
            "<row gap=1 justify=between>{cwd}{usage}</row>",
            cwd=ui.tml("<text fg=muted truncate=start>{value}</text>", value=ui.text(ctx.cwd)),
            usage=ui.tml(
                "<text fg=muted>{tokens} · {cost}</text>",
                tokens=ui.text(f"{total_tokens:,} tokens"),
                cost=ui.text(f"${facts.cost_usd:.2f}"),
            ),
        ),
        ui.SlotOptions(order=50, collapse=ui.Collapse.TRUNCATE),
        key="footer",
    )
    _last_state_hash = state_hash


@hook("extension_activate")
async def paint_activation(payload: object, ctx: omp.Context) -> None:
    """Seed the statusline when a session first activates this extension."""
    facts: StatusFacts = ctx.session.stats
    _paint(ctx, facts, facts.total_tokens)


@hook("turn_end")
async def paint_turn(payload: object, ctx: omp.Context) -> None:
    """Refresh chrome once from the settled turn's telemetry usage snapshot."""
    usage = payload.session_usage
    total_tokens = usage.input_tokens + usage.output_tokens
    facts: StatusFacts = ctx.session.stats
    _paint(ctx, facts, total_tokens)
