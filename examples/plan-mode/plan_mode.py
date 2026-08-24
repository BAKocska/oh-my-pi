"""A session-scoped planning regime with a read-only admission guard."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

import omp
from omp import ModelRef

_REGIME_ID = "plan-mode"
_GATE_REGIME_ID = "plan-settle-gate"
_PLAN_TOOLSET = ("read", "grep", "glob", "bash", "plan")
_SESSION = omp.StateScope.SESSION


@omp.entry_kind("examples.plan-mode.plan", rev="v.1")
@dataclass(frozen=True, slots=True)
class Plan:
    """Record the completed plan handed from planning into execution."""

    text: str
    model: ModelRef
    thinking: str


@dataclass(frozen=True, slots=True)
class PlanArgs:
    """Select plan-mode entry, exit, or status."""

    op: Literal["on", "off", "status"]
    model: ModelRef | None = None
    thinking: str | None = None
    plan: str | None = None


@omp.entry_kind("examples.plan-mode.selection", rev="v.1")
@dataclass(frozen=True, slots=True)
class PlanModeState:
    """Journal the model selection owned by one plan-mode activation."""

    model_provider: str
    model_api: str
    model_name: str
    thinking: str

    @classmethod
    def from_selection(cls, model: ModelRef, thinking: str) -> PlanModeState:
        """Construct durable state from a validated model selection."""

        return cls(
            model_provider=model.provider,
            model_api=model.api,
            model_name=model.model,
            thinking=thinking,
        )

    def model_ref(self) -> ModelRef:
        """Reconstruct the selected model reference."""

        return ModelRef(
            provider=self.model_provider,
            api=self.model_api,
            model=self.model_name,
        )


def _status(state: PlanModeState | None) -> dict[str, object]:
    return {
        "mode": "plan" if state is not None else "execute",
        "model": state.model_ref() if state is not None else None,
        "thinking": state.thinking if state is not None else None,
    }


async def _active_regime(regime: str) -> object | None:
    for activation in await omp.regimes.active():
        if activation.regime == regime and activation.status == "active":
            return activation
    return None


async def _active_plan_mode() -> object | None:
    return await _active_regime(_REGIME_ID)


async def _current_plan_state() -> PlanModeState | None:
    if await _active_plan_mode() is None:
        return None
    record = await omp.state.latest(PlanModeState, scope=_SESSION)
    return None if record is None else record.value


def _gate_limit(ctx: omp.RegimeContext, next_: omp.Next) -> object:
    """Complete the bounded settlement gate after three committed nudges."""

    del ctx
    return next_.complete()


@omp.regime(
    _REGIME_ID,
    on=(omp.CONTEXT, omp.PRE_MODEL, omp.ADMISSION),
    lifetime="session",
    state=PlanModeState,
    owns=("mode", "worktree"),
    sets={"toolset": _PLAN_TOOLSET, "prompt": "plan"},
    on_failure="deny",
)
def plan_mode(ctx: omp.RegimeContext, next_: omp.Next) -> object | None:
    """Apply the selected model and reject writes while plan mode is active."""

    state = ctx.state.value
    if ctx.event.point is omp.PRE_MODEL:
        ctx.settings.set(
            "model",
            {
                "provider": state.model_provider,
                "api": state.model_api,
                "model": state.model_name,
                "thinking": state.thinking,
            },
        )
    elif ctx.event.point is omp.ADMISSION and ctx.event.is_write:
        return next_.reject("plan mode is read-only")
    return None


@omp.regime(
    _GATE_REGIME_ID,
    on=omp.SETTLE,
    lifetime="run",
    max_steps=3,
    on_limit=_gate_limit,
    on_failure="defer",
)
def plan_settle_gate(ctx: omp.RegimeContext, next_: omp.Next) -> object:
    """Nudge an active planning run toward an explicit decision."""

    ctx.context.append(
        {
            "kind": "plan-mode-decision",
            "text": "Planning is active; continue using tools or finish with the plan tool.",
        }
    )
    return next_.retry()


@omp.tool("plan", kind="soft", rev=1)
async def plan(args: PlanArgs, ctx: omp.Context) -> dict[str, object]:
    """Start, stop, or inspect the plan-mode regime."""

    del ctx
    activation = await _active_plan_mode()
    if args.op == "status":
        return _status(await _current_plan_state())

    if args.op == "on":
        if activation is not None:
            raise ValueError("plan mode is already active")
        model = args.model
        thinking = (args.thinking or "").strip()
        if model is None or not thinking:
            raise ValueError("on requires model and non-empty thinking selections")
        state = PlanModeState.from_selection(model, thinking)
        handle = await omp.regimes.start(_REGIME_ID, state=state)
        try:
            await omp.state.append(state, scope=_SESSION)
            await omp.regimes.start(_GATE_REGIME_ID)
        except Exception:
            await handle.stop()
            raise
        return _status(state)

    state = await _current_plan_state()
    if activation is None or state is None:
        raise ValueError("plan mode is not active")
    text = (args.plan or "").strip()
    if not text:
        raise ValueError("off requires the completed plan")
    await omp.state.append(
        Plan(text=text, model=state.model_ref(), thinking=state.thinking),
        scope=_SESSION,
    )
    gate = await _active_regime(_GATE_REGIME_ID)
    if gate is not None:
        await omp.regimes.stop(gate.id)
    await omp.regimes.stop(activation.id)
    result = _status(None)
    result["plan"] = text
    return result
