"""A Session-scoped planning regime with a read-only campaign guard."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Literal

import omp
from omp import ModelRef

_CAMPAIGN_ID = "plan-mode"
_GATE_CAMPAIGN_ID = "plan-decision-gate"
_DENIAL_CODE = "plan_readonly"
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


@dataclass(frozen=True, slots=True)
class PlanModeState:
    """Journal the planning selection owned by one campaign engagement."""

    model_provider: str
    model_api: str
    model_name: str
    thinking: str

    @classmethod
    def from_selection(cls, model: ModelRef, thinking: str) -> PlanModeState:
        return cls(
            model_provider=model.provider,
            model_api=model.api,
            model_name=model.model,
            thinking=thinking,
        )

    def model_ref(self) -> ModelRef:
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


async def _active_plan_mode() -> omp.ActiveCampaign | None:
    for engagement in await omp.campaigns.active():
        if engagement.campaign != _CAMPAIGN_ID or engagement.queued:
            continue
        if isinstance(engagement.state, PlanModeState):
            return engagement
    return None


async def _active_gate() -> omp.ActiveCampaign | None:
    for engagement in await omp.campaigns.active():
        if engagement.campaign == _GATE_CAMPAIGN_ID and not engagement.queued:
            return engagement
    return None


def _tool_name(event: Mapping[str, object]) -> str | None:
    target = event.get("target")
    if isinstance(target, str):
        return target
    if isinstance(target, Mapping):
        name = target.get("name")
        return name if isinstance(name, str) else None
    return None


def _bash_is_read_only(value: object) -> bool:
    if not isinstance(value, Mapping):
        return True
    commands = value.get("commands", ())
    commands_are_read_only = (
        isinstance(commands, Sequence)
        and not isinstance(commands, (str, bytes))
        and all(
            isinstance(command, Mapping) and bool(command.get("read_only"))
            for command in commands
        )
    )
    return (
        not bool(value.get("writes"))
        and not bool(value.get("net"))
        and not bool(value.get("has_dynamic_eval"))
        and commands_are_read_only
    )


def _is_mutating_call(event: Mapping[str, object]) -> bool:
    kind = event.get("kind")
    if kind not in (None, "core"):
        return False
    tool = _tool_name(event)
    if tool in {"write", "edit"}:
        return True
    return tool == "bash" and not _bash_is_read_only(event.get("bash"))


@omp.campaign(
    _CAMPAIGN_ID,
    at=(omp.CONTEXT, omp.PRE_MODEL, omp.ADMISSION),
    scope=omp.CampaignScope.SESSION,
    state=PlanModeState,
    state_family="examples.plan-mode.state",
    on_failure=omp.OnFailure.DENY,
    claims=("mode", "worktree"),
    binds=("toolset", "model"),
)
def plan_mode(
    event: dict[str, object],
    state: PlanModeState,
) -> tuple[object, PlanModeState]:
    """Bind planning surfaces, guard writes."""

    point = event.get("point")
    if point == omp.CONTEXT.value:
        return omp.Bind("toolset", _PLAN_TOOLSET), state
    if point == omp.PRE_MODEL.value:
        return (
            omp.Bind(
                "model",
                {
                    "provider": state.model_provider,
                    "api": state.model_api,
                    "model": state.model_name,
                    "thinking": state.thinking,
                },
            ),
            state,
        )
    if point == omp.ADMISSION.value and _is_mutating_call(event):
        return omp.Deny("plan is read-only", code=_DENIAL_CODE), state
    return omp.Pass(), state

@omp.campaign(
    _GATE_CAMPAIGN_ID,
    at=omp.SETTLE,
    ladder=omp.Ladder(3),
    exhaust=omp.Exhaust.SETTLE,
    scope=omp.CampaignScope.RUN,
)
def plan_decision_gate(event: dict[str, object]) -> object:
    """Nudge an active planning run toward an explicit decision."""

    del event
    return omp.Continue(
        inject={
            "kind": "plan-mode-decision",
            "text": "Planning is active; continue using tools or finish with the plan tool.",
        }
    )


@omp.tool("plan", kind="soft", rev=1)
async def plan(args: PlanArgs, ctx: omp.Context) -> dict[str, object]:
    """Engage, disengage, or inspect the plan-mode campaign."""

    del ctx
    engagement = await _active_plan_mode()
    if args.op == "status":
        state = engagement.state if engagement is not None else None
        return _status(state if isinstance(state, PlanModeState) else None)

    if args.op == "on":
        if engagement is not None:
            raise ValueError("plan mode is already active")
        model = args.model
        thinking = (args.thinking or "").strip()
        if model is None or not thinking:
            raise ValueError("on requires model and non-empty thinking selections")
        active = await omp.campaigns.engage(
            _CAMPAIGN_ID,
            state=PlanModeState.from_selection(model, thinking),
        )
        try:
            await omp.campaigns.engage(_GATE_CAMPAIGN_ID)
        except Exception:
            await omp.campaigns.disengage(active.id)
            raise
        state = active.state
        return _status(state if isinstance(state, PlanModeState) else None)

    if engagement is None or not isinstance(engagement.state, PlanModeState):
        raise ValueError("plan mode is not active")
    text = (args.plan or "").strip()
    if not text:
        raise ValueError("off requires the completed plan")
    state = engagement.state
    await omp.state.append(
        Plan(text=text, model=state.model_ref(), thinking=state.thinking),
        scope=_SESSION,
    )
    gate = await _active_gate()
    if gate is not None and not await omp.campaigns.disengage(gate.id):
        raise RuntimeError("plan-decision-gate campaign could not be disengaged")
    if not await omp.campaigns.disengage(engagement.id):
        raise RuntimeError("plan-mode campaign could not be disengaged")
    result = _status(None)
    result["plan"] = text
    return result
