"""Project-scoped model pinning with a session-local manual override latch."""

from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import ui

_PROJECT = omp.StateScope.PROJECT
_SESSION = omp.StateScope.SESSION


@omp.entry_kind("examples.project-model-pin.pin", rev="v.1", display=False)
@dataclass(frozen=True, slots=True)
class ModelPin:
    """Remember the selected model for this principal and workspace."""

    model: omp.ModelRef


@omp.entry_kind("examples.project-model-pin.session-latch", rev="v.1", display=False)
@dataclass(frozen=True, slots=True)
class PinLatch:
    """Record whether this session may still apply the project pin."""

    enabled: bool


def _value(record: object | None, kind: type[object]) -> object | None:
    """Return a decoded state value only when it has the expected type."""

    value = getattr(record, "value", None)
    return value if isinstance(value, kind) else None


async def _project_pin() -> ModelPin | None:
    """Read the latest project model pin."""

    record = await omp.state.latest(ModelPin, scope=_PROJECT)
    value = _value(record, ModelPin)
    return value if isinstance(value, ModelPin) else None


async def _pin_is_enabled() -> bool:
    """Read the session latch, defaulting new sessions to enabled."""

    record = await omp.state.latest(PinLatch, scope=_SESSION)
    value = _value(record, PinLatch)
    return True if value is None else value.enabled


def _model_patch(
    pin: ModelPin | None, enabled: bool, current: omp.ModelRef
) -> dict[str, omp.ModelRef] | None:
    """Return the model replacement required by the current pin state."""

    if pin is None or not enabled or pin.model == current:
        return None
    return {"model": pin.model}


@omp.command(
    "pin_model",
    description="Pin the active model to this project",
)
async def pin_model(inv: ui.Invocation, ctx: omp.Context) -> ui.Consumed:
    """Pin the active model for the current principal and workspace."""

    if inv.argv:
        return ui.Consumed(ui.text("Usage: /pin_model"))
    if ctx.model is None:
        return ui.Consumed(ui.text("No active model to pin."))

    await omp.state.append(ModelPin(ctx.model), scope=_PROJECT)
    await omp.state.append(PinLatch(enabled=True), scope=_SESSION)
    selected = ctx.model
    return ui.Consumed(
        ui.text(
            f"Pinned project model: {selected.provider}/{selected.model} "
            f"({selected.api})"
        )
    )


@omp.hook(
    "model_changed",
    phase=omp.HookPhase.OBSERVE,
)
async def remember_manual_override(
    event: omp.ModelChangedEvent, ctx: omp.Context
) -> None:
    """Clear the session latch when the user manually changes model."""

    del ctx
    if event.reason is omp.ModelChangeReason.USER:
        await omp.state.append(PinLatch(enabled=False), scope=_SESSION)


@omp.hook(
    "turn_start",
    phase=omp.HookPhase.TRANSFORM,
    order=40,
    on_failure=omp.OnFailure.DEFER,
)
async def apply_project_pin(
    event: omp.TurnStartEvent, ctx: omp.Context
) -> omp.HookDecision:
    """Apply the project model unless this session has a manual override."""

    del ctx
    patch = _model_patch(await _project_pin(), await _pin_is_enabled(), event.model)
    if patch is None:
        return omp.Defer()
    return omp.Modify(patch=patch, reason="project model pin")
