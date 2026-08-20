from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import ui


_CATALOG_NOTICE_CAVEAT = (
    "Catalog notification tokens are counted in message_tokens; the frozen "
    "ContextUsage does not yet expose the ruled separate catalog_notice_tokens echo."
)
_latest_prompts: dict[str, omp.PromptFingerprint] = {}


@dataclass(frozen=True, slots=True)
class SlotBytes:
    """Record assembler-owned byte and stability facts for one prompt slot."""

    name: str
    size_bytes: int
    band: str
    digest: str


@omp.entry_kind("examples.context-report.report", rev="v.1", spill=False)
@dataclass(frozen=True, slots=True)
class ContextReport:
    """Store one user-visible, model-invisible context attribution report."""

    total_tokens: int
    context_window: int
    reserve_tokens: int
    usable_tokens: int
    fraction: float
    prompt_head_tokens: int
    device_catalog_tokens: int
    message_tokens: int
    media_tokens: int
    compaction_epoch: int
    threshold_fraction: float
    in_flight: bool
    prompt_digest: str
    slots: tuple[SlotBytes, ...]
    catalog_notice_accounting: str

    def render(self, ctx: ui.RenderCtx) -> ui.Tml:
        """Render the durable report without defining a model projection."""

        del ctx
        slot_rows = ui.join(
            (
                ui.tml(
                    "<row gap=1><text fg=muted>{band}</text><text>{name}</text>"
                    "<text fg=secondary>{size}</text></row>",
                    band=slot.band,
                    name=slot.name,
                    size=f"{slot.size_bytes:,} B",
                )
                for slot in self.slots
            )
        )
        return ui.tml(
            "<column gap=1><row gap=1><text fg=accent>Context</text>"
            "<text>{used}</text><text fg=muted>of {usable} usable "
            "({fraction})</text></row>"
            "<row gap=1><text>prompt {prompt}</text><text>catalog {catalog}</text>"
            "<text>messages {messages}</text><text>media {media}</text></row>"
            "<row gap=1><text fg=muted>window {window}; reserve {reserve}; "
            "threshold {threshold}; epoch {epoch}; in-flight {in_flight}</text></row>"
            "<row gap=1><text fg=muted>prompt {digest}</text></row>"
            "{slots}<text fg=muted>{caveat}</text></column>",
            used=f"{self.total_tokens:,} tokens",
            usable=f"{self.usable_tokens:,}",
            fraction=f"{self.fraction:.1%}",
            prompt=f"{self.prompt_head_tokens:,}",
            catalog=f"{self.device_catalog_tokens:,}",
            messages=f"{self.message_tokens:,}",
            media=f"{self.media_tokens:,}",
            window=f"{self.context_window:,}",
            reserve=f"{self.reserve_tokens:,}",
            threshold=f"{self.threshold_fraction:.1%}",
            epoch=self.compaction_epoch,
            in_flight=str(self.in_flight).lower(),
            digest=self.prompt_digest or "unavailable before first prompt",
            slots=slot_rows,
            caveat=self.catalog_notice_accounting,
        )


def _report(usage: omp.ContextUsage, prompt: omp.PromptFingerprint | None) -> ContextReport:
    """Copy host-owned usage and fingerprint fields into a durable report."""

    slots = () if prompt is None else tuple(
        SlotBytes(
            name=name,
            size_bytes=facts.size_bytes,
            band=facts.band.value,
            digest=facts.digest,
        )
        for name, facts in prompt.slots.items()
    )
    return ContextReport(
        total_tokens=usage.total_tokens,
        context_window=usage.context_window,
        reserve_tokens=usage.reserve_tokens,
        usable_tokens=usage.usable_tokens,
        fraction=usage.fraction,
        prompt_head_tokens=usage.prompt_head_tokens,
        device_catalog_tokens=usage.device_catalog_tokens,
        message_tokens=usage.message_tokens,
        media_tokens=usage.media_tokens,
        compaction_epoch=usage.compaction_epoch,
        threshold_fraction=usage.threshold_fraction,
        in_flight=usage.in_flight,
        prompt_digest="" if prompt is None else prompt.digest,
        slots=slots,
        catalog_notice_accounting=_CATALOG_NOTICE_CAVEAT,
    )


def _set_pressure_badge(usage: omp.ContextUsage) -> None:
    """Paint severity solely from the host-maintained pressure fields."""

    tone = (
        ui.Token.ERR
        if usage.fraction >= 1.0
        else ui.Token.WARN
        if usage.fraction >= usage.threshold_fraction
        else ui.Token.MUTED
    )
    ui.set_status(
        "context-pressure",
        ui.tml(
            "<segment fg={tone}>{label}</segment>",
            tone=tone,
            label=f"ctx {usage.fraction:.0%}",
        ),
        order=40,
        side=ui.Slot.STATUS_RIGHT,
    )


@omp.telemetry(
    kinds=["session_start", "model_request", "turn_end"],
    scope=omp.telemetry.Scope.SELF,
    queue=64,
    overflow=omp.telemetry.Overflow.DROP_OLDEST,
)
async def pressure(payload: object, ctx: omp.Context) -> None:
    """Refresh the latest fingerprint and coalesced context-pressure badge."""

    prompt = getattr(payload, "prompt", None)
    if isinstance(prompt, omp.PromptFingerprint):
        _latest_prompts[ctx.session] = prompt
    _set_pressure_badge(await omp.context.usage())


@ui.command("context", description="Show the current context attribution report")
async def context_command(args: ui.Invocation, ctx: omp.Context) -> ui.Consumed:
    """Append a rendered journal report that has no model-facing projection."""

    if args.argv:
        return ui.Consumed(ui.text("Usage: /context"))
    usage = await omp.context.usage()
    omp.journal.append(_report(usage, _latest_prompts.get(ctx.session)), display=True)
    return ui.Consumed()
