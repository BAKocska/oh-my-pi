from __future__ import annotations

from collections.abc import Hashable
from dataclasses import dataclass
from typing import cast

import omp
from omp import ui

_EMA_ALPHA = 0.25
_MIN_DECODE_MS = 300
_TPS_FAST = 50.0
_TPS_MEDIUM = 20.0
_TTFT_FAST_MS = 500.0
_TTFT_MEDIUM_MS = 2_000.0


@dataclass(slots=True)
class _ModelEma:
    """Hold rebuildable response metrics for one served model."""

    tps: float | None = None
    ttft_ms: float | None = None
    wall_ms: float | None = None
    samples: int = 0
    dropped: int = 0
    coalesced: int = 0
    last_loss: int = 0


_models: dict[str, _ModelEma] = {}
_drop_totals = (0, 0)


def _ema(previous: float | None, sample: float) -> float:
    """Fold one sample into the configured exponential moving average."""

    if previous is None:
        return sample
    return previous + _EMA_ALPHA * (sample - previous)


def _coalesce_model(request: object) -> Hashable:
    """Coalesce queued request updates by the model that served them."""

    return str(getattr(request, "served_model"))


def _counter_delta(current: int, previous: int) -> int:
    """Measure a counter window while tolerating host-generation resets."""

    return current - previous if current >= previous else current


def _fold(request: omp.telemetry.ModelRequest, stats: omp.telemetry.DropStats) -> _ModelEma:
    """Update one model EMA strictly from settled telemetry fields."""

    global _drop_totals

    model = request.served_model
    state = _models.setdefault(model, _ModelEma())
    latency_ms = float(request.latency_ms)
    ttft_ms = None if request.ttft_ms is None else float(request.ttft_ms)

    state.wall_ms = _ema(state.wall_ms, latency_ms)
    if ttft_ms is not None:
        state.ttft_ms = _ema(state.ttft_ms, ttft_ms)
        decode_ms = latency_ms - ttft_ms
        if decode_ms >= _MIN_DECODE_MS:
            tps = request.usage.output * 1_000.0 / decode_ms
            state.tps = _ema(state.tps, tps)

    previous_dropped, previous_coalesced = _drop_totals
    state.last_loss = _counter_delta(stats.dropped, previous_dropped) + _counter_delta(
        stats.coalesced, previous_coalesced
    )
    state.dropped = stats.dropped
    state.coalesced = stats.coalesced
    _drop_totals = (stats.dropped, stats.coalesced)
    state.samples += 1
    return state


def _tps_tone(tps: float | None) -> ui.Token:
    """Map decode throughput onto semantic theme severity."""

    if tps is None:
        return ui.Token.MUTED
    if tps >= _TPS_FAST:
        return ui.Token.OK
    if tps >= _TPS_MEDIUM:
        return ui.Token.WARN
    return ui.Token.ERR


def _ttft_tone(ttft_ms: float | None) -> ui.Token:
    """Map time to first token onto semantic theme severity."""

    if ttft_ms is None:
        return ui.Token.MUTED
    if ttft_ms <= _TTFT_FAST_MS:
        return ui.Token.OK
    if ttft_ms <= _TTFT_MEDIUM_MS:
        return ui.Token.WARN
    return ui.Token.ERR


def _metric(value: float | None, suffix: str, *, scale: float = 1.0) -> str:
    """Format one compact footer metric without terminal styling."""

    return "— " + suffix if value is None else f"{value / scale:.1f}{suffix}"


def _paint(state: _ModelEma) -> None:
    """Replace the retained footer segment with the newest EMA snapshot."""

    ui.mount(
        ui.Slot.FOOTER,
        ui.tml(
            "<row gap=1><text fg={tps_tone}>{tps}</text><text fg=muted>·</text>"
            "<text fg={ttft_tone}>{ttft}</text><text fg=muted>·</text>"
            "<text fg={last_tone}>{last}</text></row>",
            tps_tone=_tps_tone(state.tps),
            tps=ui.text(_metric(state.tps, " tps")),
            ttft_tone=_ttft_tone(state.ttft_ms),
            ttft=ui.text(_metric(state.ttft_ms, "s ttft", scale=1_000.0)),
            last_tone=ui.Token.ERR if state.last_loss else ui.Token.MUTED,
            last=ui.text(_metric(state.wall_ms, "s last", scale=1_000.0)),
        ),
        ui.SlotOptions(order=60, collapse=ui.Collapse.TRUNCATE),
        key="pulse.metrics",
    )


@omp.telemetry(
    [omp.telemetry.Kind.MODEL_REQUEST],
    scope=omp.telemetry.Scope.TREE,
    queue=128,
    overflow=omp.telemetry.Overflow.COALESCE_BY_KEY,
    coalesce_key=_coalesce_model,
)
async def observe_requests(request: omp.telemetry.ModelRequest, ctx: omp.Context) -> None:
    """Fold a settled model request and render its model's footer EMA."""

    del ctx
    stats = cast(omp.telemetry.DropStats, omp.telemetry.dropped(observe_requests))
    state = _fold(request, stats)
    _paint(state)
