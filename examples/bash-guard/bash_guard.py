"""A BashIR-backed shell guardian with a session-scoped rejection breaker."""

from __future__ import annotations

from dataclasses import dataclass

from omp import Duration, StateScope, state
from omp import BashIR  # GAP: not in frozen layer yet (docs/py/06 §BashIR)
from omp import Context
from omp import Defer
from omp import Deny
from omp import HookDecision
from omp import HookPhase
from omp import OnFailure
from omp import ToolCallEvent
from omp import When
from omp import agents
from omp import entry_kind
from omp import hook
from omp import journal

_BREAKER_TRIP = 3
_SESSION = StateScope.SESSION
_BASH_ONLY = When(name=frozenset({"bash"}))


@entry_kind("examples.bash-guard.breaker-state", rev="v.1", display=False)
@dataclass(frozen=True, slots=True)
class BreakerState:
    """Record the guardian's consecutive rejections and open state."""

    rejections: int
    open: bool


@entry_kind("examples.bash-guard.breaker-warning", rev="v.1", display=False)
@dataclass(frozen=True, slots=True)
class BreakerWarning:
    """Record that the open breaker deliberately disabled guardian denials."""

    rejections: int
    reason: str


@dataclass(frozen=True, slots=True)
class _Transition:
    """Carry a REVIEW result to the effect-capable OBSERVE phase."""

    state: BreakerState


_pending: dict[object, _Transition] = {}


def _needs_review(ir: BashIR) -> bool:
    """Require review unless a successful static analysis proves read-only behavior."""

    if not ir.parse_ok or ir.has_dynamic_eval:
        return True
    return not ir.is_read_only()


async def _current_breaker() -> BreakerState:
    """Read the latest session-scoped breaker snapshot through frozen omp.state."""

    record = await state.latest(BreakerState, scope=_SESSION)
    if record is None:
        return BreakerState(rejections=0, open=False)
    return record.value


@hook(
    "tool_call",
    phase=HookPhase.PRECHECK,
    on_failure=OnFailure.DENY,
    timeout=Duration("100ms"),
    when=_BASH_ONLY,
)
async def bash_precheck(event: ToolCallEvent, ctx: Context) -> HookDecision:
    """Apply only deterministic BashIR fast paths and leave ambiguity for REVIEW."""

    del ctx
    ir = event.bash
    if ir is None:
        return Defer()
    if not ir.parse_ok or ir.has_dynamic_eval:
        return Defer()
    if ir.is_read_only():
        return Defer()
    return Defer()


@hook(
    "tool_call",
    phase=HookPhase.REVIEW,
    on_failure=OnFailure.DENY,
    timeout=Duration("3s"),
    when=_BASH_ONLY,
)
async def review_bash(event: ToolCallEvent, ctx: Context) -> HookDecision:
    """Classify non-read-only BashIR facts and stop denying when the breaker opens."""

    del ctx
    ir = event.bash
    if ir is None or not _needs_review(ir):
        return Defer()

    breaker = await _current_breaker()
    if breaker.open:
        _pending[event.call_id] = _Transition(state=breaker)
        return Defer()

    review = await agents.completion(
        {
            "script": ir.source,
            "parse_ok": ir.parse_ok,
            "dynamic_eval": ir.has_dynamic_eval,
            "read_only": False,
        },
        role="smol",
        system="Classify this shell command's risk. Answer allow, review, or deny.",
        choices=("allow", "review", "deny"),
        default="review",
        deadline=Duration("2s"),
        labels={"gate": "bash-guard"},
    )

    match review.choice:
        case "allow":
            if breaker.rejections:
                _pending[event.call_id] = _Transition(
                    state=BreakerState(rejections=0, open=False)
                )
            return Defer()
        case "deny":
            rejections = breaker.rejections + 1
            opened = rejections >= _BREAKER_TRIP
            _pending[event.call_id] = _Transition(
                state=BreakerState(rejections=rejections, open=opened)
            )
            if opened:
                return Defer()
            return Deny(review.text, code="bash_guard.denied")
        case _:
            return Defer()


@hook(
    "tool_call",
    phase=HookPhase.OBSERVE,
    timeout=Duration("1s"),
    when=_BASH_ONLY,
)
async def persist_breaker(event: ToolCallEvent, ctx: Context) -> None:
    """Persist REVIEW transitions and journal one warning after fail-open."""

    del ctx
    transition = _pending.pop(event.call_id, None)
    if transition is not None:
        await state.append(
            transition.state,
            scope=_SESSION,
            idempotency_key=f"bash-guard:{event.call_id}",
        )
        breaker = transition.state
    else:
        breaker = await _current_breaker()

    if breaker.open:
        await journal.append(
            BreakerWarning(
                rejections=breaker.rejections,
                reason="guardian circuit breaker is open; denials are disabled",
            ),
            idempotency_key="bash-guard:breaker-open",
        )
