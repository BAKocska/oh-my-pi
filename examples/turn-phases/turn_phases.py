from __future__ import annotations

from dataclasses import dataclass, replace
from enum import IntEnum
from hashlib import blake2s

import omp
from omp import ui
from omp.provider import RequestDraft

STREAM_WINDOW = omp.Duration("50ms")
_MAX_COUNT = 999
_MAX_HISTORY = 32


class TurnPhase(IntEnum):
    """Order the typed lifecycle boundaries shown in the working banner."""

    WAITING = 0
    TURN_STARTED = 10
    REQUEST_ADMITTED = 20
    RESPONSE_STREAMING = 30
    CALL_DISCOVERED = 35
    REQUEST_SETTLED = 40
    TOOL_RUNNING = 50
    TOOL_SETTLED = 60
    COMPACTED = 70
    TURN_SETTLED = 80


_PHASE_LABEL = {
    TurnPhase.WAITING: "Preparing",
    TurnPhase.TURN_STARTED: "Starting turn",
    TurnPhase.REQUEST_ADMITTED: "Request admitted",
    TurnPhase.RESPONSE_STREAMING: "Streaming response",
    TurnPhase.CALL_DISCOVERED: "Preparing tool call",
    TurnPhase.REQUEST_SETTLED: "Response settled",
    TurnPhase.TOOL_RUNNING: "Running tool",
    TurnPhase.TOOL_SETTLED: "Tool settled",
    TurnPhase.COMPACTED: "Context compacted",
    TurnPhase.TURN_SETTLED: "Turn settled",
}


@dataclass(frozen=True, slots=True)
class StatusFacts:
    """Hold the bounded facts that determine one working-message render."""

    phase: TurnPhase = TurnPhase.WAITING
    turn_id: str | None = None
    attempt: int = 1
    active_tools: int = 0
    settled_tools: int = 0
    degraded: int = 0
    refused: int = 0
    compactions: int = 0


_facts = StatusFacts()
_last_render_hash: bytes | None = None
_phase_history: tuple[TurnPhase, ...] = ()
_effect_count = 0


def status_facts() -> StatusFacts:
    """Return the current immutable status snapshot."""

    return _facts


def phase_history() -> tuple[TurnPhase, ...]:
    """Return the bounded sequence of emitted lifecycle phases."""

    return _phase_history


def effect_count() -> int:
    """Return the number of non-redundant working-message effects emitted."""

    return _effect_count


def _bounded(value: int) -> int:
    return min(max(value, 0), _MAX_COUNT)


def _digest(facts: StatusFacts) -> bytes:
    values = (
        int(facts.phase),
        facts.turn_id or "",
        facts.attempt,
        facts.active_tools,
        facts.settled_tools,
        facts.degraded,
        facts.refused,
        facts.compactions,
    )
    return blake2s(
        b"\0".join(str(value).encode("utf-8") for value in values),
        digest_size=16,
    ).digest()


def _label(facts: StatusFacts) -> str:
    parts = [_PHASE_LABEL[facts.phase]]
    if facts.attempt > 1:
        parts.append(f"retry {facts.attempt}")
    if facts.active_tools:
        parts.append(f"{facts.active_tools} active")
    if facts.settled_tools:
        parts.append(f"{facts.settled_tools} settled")
    if facts.degraded or facts.refused:
        parts.append(f"{facts.degraded} degraded/{facts.refused} refused")
    return " · ".join(parts)


def _paint(facts: StatusFacts) -> bool:
    global _effect_count, _last_render_hash

    render_hash = _digest(facts)
    if render_hash == _last_render_hash:
        return False

    ui.set_working_message(
        ui.tml(
            "<row gap=1><spinner/><text shimmer>{label}</text></row>",
            label=ui.text(_label(facts)),
        )
    )
    _last_render_hash = render_hash
    _effect_count += 1
    return True


def _advance(phase: TurnPhase, **changes: object) -> bool:
    global _facts, _phase_history

    next_facts = replace(_facts, **changes)
    if phase >= next_facts.phase:
        next_facts = replace(next_facts, phase=phase)
    _facts = next_facts
    changed = _paint(next_facts)
    if changed and (not _phase_history or _phase_history[-1] != next_facts.phase):
        _phase_history = (*_phase_history[-(_MAX_HISTORY - 1) :], next_facts.phase)
    return changed


def _reset_turn(turn_id: str, attempt: int) -> bool:
    global _facts

    _facts = StatusFacts(
        phase=TurnPhase.TURN_STARTED,
        turn_id=turn_id,
        attempt=max(attempt, 1),
        compactions=_facts.compactions,
    )
    return _advance(TurnPhase.TURN_STARTED)


def _restore_builtin() -> bool:
    global _effect_count, _last_render_hash

    if _last_render_hash is None:
        return False
    ui.set_working_message(None)
    _last_render_hash = None
    _effect_count += 1
    return True


@omp.hook("turn_start", phase=omp.HookPhase.OBSERVE)
async def observe_turn_start(event: omp.TurnStartEvent, ctx: omp.Context) -> None:
    """Start a fresh status fold from the typed turn identifier and attempt."""

    _reset_turn(event.turn_id, event.attempt)


@omp.hook("before_request", phase=omp.HookPhase.OBSERVE)
async def observe_request_admitted(event: RequestDraft, ctx: omp.Context) -> None:
    """Mark the provider request admitted without inspecting request text."""

    _advance(TurnPhase.REQUEST_ADMITTED)


@omp.hook("message_start", coalesce=STREAM_WINDOW)
async def observe_message_start(event: omp.MessageStartEvent, ctx: omp.Context) -> None:
    """Enter response streaming from the typed message-start boundary."""

    _advance(TurnPhase.RESPONSE_STREAMING)


@omp.hook("message_update", coalesce=STREAM_WINDOW)
async def observe_message_update(event: omp.MessageUpdateEvent, ctx: omp.Context) -> None:
    """Keep response streaming current once per declared coalesce window."""

    _advance(TurnPhase.RESPONSE_STREAMING)


@omp.hook("call_open", coalesce=STREAM_WINDOW)
async def observe_call_open(event: omp.CallOpenEvent, ctx: omp.Context) -> None:
    """Name tool preparation from the typed speculative-call boundary."""

    _advance(TurnPhase.CALL_DISCOVERED)


@omp.hook("message_end", coalesce=STREAM_WINDOW)
async def observe_request_settled(event: omp.MessageEndEvent, ctx: omp.Context) -> None:
    """Mark the streamed provider response settled from its typed finish event."""

    _advance(TurnPhase.REQUEST_SETTLED)


@omp.hook("tool_execution_start")
async def observe_tool_start(event: omp.ToolExecutionStartEvent, ctx: omp.Context) -> None:
    """Count one admitted tool invocation at executor start."""

    _advance(TurnPhase.TOOL_RUNNING, active_tools=_bounded(_facts.active_tools + 1))


@omp.hook("tool_update", coalesce=STREAM_WINDOW)
async def observe_tool_update(event: omp.ToolUpdateEvent, ctx: omp.Context) -> None:
    """Retain the running phase without folding per-update payload text."""

    _advance(TurnPhase.TOOL_RUNNING)


@omp.hook("tool_execution_end")
async def observe_tool_end(event: omp.ToolExecutionEndEvent, ctx: omp.Context) -> None:
    """Fold executor settlement into bounded active and settled counts."""

    _advance(
        TurnPhase.TOOL_SETTLED,
        active_tools=_bounded(_facts.active_tools - 1),
        settled_tools=_bounded(_facts.settled_tools + 1),
    )


@omp.hook("capability_budget")
async def observe_degradation(event: omp.CapabilityBudgetEvent, ctx: omp.Context) -> None:
    """Record typed inference degradations and refusals without parsing prose."""

    _advance(
        _facts.phase,
        degraded=_bounded(_facts.degraded + len(event.degraded)),
        refused=_bounded(_facts.refused + len(event.refused)),
    )


@omp.hook("compaction_done")
async def observe_compaction(event: omp.CompactionOutcome, ctx: omp.Context) -> None:
    """Mark one completed typed compaction outcome."""

    _advance(
        TurnPhase.COMPACTED,
        compactions=_bounded(_facts.compactions + 1),
    )


@omp.hook("turn_end")
async def observe_turn_end(event: omp.TurnEndEvent, ctx: omp.Context) -> None:
    """Show the final typed turn boundary before submission settlement."""

    _advance(TurnPhase.TURN_SETTLED)


@omp.hook("agent_end")
async def observe_agent_end(event: omp.AgentEndEvent, ctx: omp.Context) -> None:
    """Restore omp's built-in loader after the caller submission ends."""

    _restore_builtin()
