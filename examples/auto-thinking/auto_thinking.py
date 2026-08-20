"""Adjust turn reasoning effort with a bounded difficulty classifier."""

from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import agents, context

_CHOICES = ("minimal", "low", "medium", "high", "xhigh")
_EFFORT_INDEX = {effort: index for index, effort in enumerate(omp.Effort)}
_COMPLEX_MARKERS = (
    "architecture",
    "concurrent",
    "debug",
    "end-to-end",
    "implement",
    "investigate",
    "migrate",
    "race condition",
    "refactor",
    "root cause",
    "security",
    "thoroughly",
)
_SIMPLE_MARKERS = ("briefly", "define ", "list ", "quick", "say ", "translate ", "what is ")


@dataclass(frozen=True, slots=True)
class _TaskEvidence:
    preview: str
    byte_len: int
    tokens: int
    part_count: int
    media_count: int


def _latest_task(view: context.ContextView) -> _TaskEvidence:
    """Return bounded facts for the latest projected user request."""

    for message in reversed(view.messages):
        if message.kind is context.MessageKind.USER:
            return _TaskEvidence(
                preview=message.preview,
                byte_len=message.byte_len,
                tokens=message.tokens,
                part_count=message.part_count,
                media_count=message.media_count,
            )
    return _TaskEvidence("", 0, 0, 0, 0)


def _heuristic_difficulty(task: _TaskEvidence) -> omp.Effort:
    """Choose a conservative deterministic level when classification fails."""

    text = " ".join(task.preview.casefold().split())
    score = 2
    complex_hits = sum(marker in text for marker in _COMPLEX_MARKERS)

    if task.byte_len >= 800 or task.tokens >= 200 or task.part_count >= 4:
        score += 1
    if task.byte_len >= 2_400 or task.tokens >= 600 or task.media_count:
        score += 1
    if complex_hits:
        score += 1
    if complex_hits >= 2:
        score += 1

    simple = any(marker in text for marker in _SIMPLE_MARKERS)
    if simple and not complex_hits and task.byte_len <= 240 and task.part_count <= 1:
        score = 1
    if simple and task.byte_len <= 48 and task.tokens <= 16:
        score = 0

    return omp.Effort(_CHOICES[min(score, len(_CHOICES) - 1)])


def _clamp_to_capability_evidence(
    requested: omp.Effort, current: omp.Effort
) -> omp.Effort | None:
    """Clamp to capability positively evidenced by the current selection."""

    if current is omp.Effort.OFF:
        return None
    if _EFFORT_INDEX[requested] > _EFFORT_INDEX[current]:
        return current
    return requested


async def _classify(task: _TaskEvidence) -> omp.Effort:
    """Classify bounded task facts through the smol-role choices ladder."""

    fallback = _heuristic_difficulty(task)
    answer = await agents.completion(
        {
            "preview": task.preview,
            "bytes": task.byte_len,
            "tokens": task.tokens,
            "parts": task.part_count,
            "media": task.media_count,
        },
        role="smol",
        system=(
            "Classify task difficulty by the required reasoning effort. "
            "Answer minimal, low, medium, high, or xhigh."
        ),
        choices=_CHOICES,
        default=fallback.value,
        max_output_tokens=8,
        deadline=omp.Duration("2s"),
        labels={"feature": "auto-thinking"},
    )
    choice = answer.choice if answer.choice in _CHOICES else fallback.value
    return omp.Effort(choice)


@omp.hook(
    "turn_start",
    phase=omp.HookPhase.TRANSFORM,
    order=60,
    on_failure=omp.OnFailure.DEFER,
    timeout=omp.Duration("3s"),
)
async def adjust_thinking(
    event: omp.TurnStartEvent, ctx: omp.Context
) -> omp.HookDecision:
    """Patch this turn to the bounded task-appropriate thinking effort."""

    del ctx
    requested = await _classify(_latest_task(await context.view()))
    selected = _clamp_to_capability_evidence(requested, event.thinking)
    if selected is None or selected is event.thinking:
        return omp.Defer(note="model thinking capability does not permit a change")
    return omp.Modify(
        patch={"thinking": selected},
        reason="task difficulty classifier selected turn thinking effort",
    )
