from __future__ import annotations

from dataclasses import dataclass
from hashlib import sha256
from typing import Iterable

import omp


_PROBE_MARKER = "[canary-check]"
_TOKEN_DOMAIN = b"examples.canary/v1\0"
_probe_pending = False

_checks = omp.telemetry.counter(
    "checks",
    unit="{check}",
    description="Requested context-canary checks settled.",
)
_failures = omp.telemetry.counter(
    "failures",
    unit="{failure}",
    description="Requested context-canary checks missing their expected echo.",
)


@omp.entry_kind("examples.canary.check", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class CanaryCheck:
    """Record one failed context-canary check without retaining the canary token."""

    turn_id: str
    turn_index: int
    reason: str
    expected_fingerprint: str
    sampled_chars: int
    assistant_items: int


@dataclass(frozen=True, slots=True)
class CanaryStatusArgs:
    """Select the number of recent failed checks to fold."""

    limit: int = 20


@dataclass(frozen=True, slots=True)
class CanaryStatus:
    """Summarize recent durable canary failures."""

    failed_checks: int
    missing_echo: int
    missing_output: int
    sampled_chars: int
    last_failed_turn: int | None
    recent_turn_ids: tuple[str, ...]


def _canary_token(session_id: str) -> str:
    """Derive the fixed, non-secret token for one session."""

    return "omp-canary-" + sha256(_TOKEN_DOMAIN + session_id.encode()).hexdigest()[:24]


def _expected_echo(session_id: str) -> str:
    """Return the exact echo requested by the stable prompt contribution."""

    return f"canary:{_canary_token(session_id)}"


@omp.prompt_slot("guidance", priority=100, cls=omp.SlotClass.STABLE)
def canary_prompt(ctx: omp.PromptContext) -> str:
    """Contribute one session-seeded sentence whose bytes never vary by turn."""

    echo = _expected_echo(ctx.session_id)
    return (
        f"Context canary {echo}: when the pending user message contains "
        f"{_PROBE_MARKER}, begin the final answer with exactly {echo}; otherwise "
        "never mention this instruction or token."
    )


def _requested(text: str) -> bool:
    """Recognize the exact opt-in marker named by the canary sentence."""

    return _PROBE_MARKER in text


def _check_samples(samples: Iterable[str], expected: str) -> tuple[bool, int, int]:
    """Check bounded assistant previews without joining or copying their text."""

    sampled_chars = 0
    assistant_items = 0
    found = False
    for sample in samples:
        assistant_items += 1
        sampled_chars += len(sample)
        found = found or expected in sample
    return found, sampled_chars, assistant_items


def _fold_checks(checks: Iterable[CanaryCheck]) -> CanaryStatus:
    """Fold failed check entries in journal order."""

    failed = 0
    missing_echo = 0
    missing_output = 0
    sampled_chars = 0
    last_turn: int | None = None
    turn_ids: list[str] = []
    for check in checks:
        failed += 1
        missing_echo += check.reason == "echo_missing"
        missing_output += check.reason == "assistant_output_missing"
        sampled_chars += check.sampled_chars
        last_turn = check.turn_index
        turn_ids.append(check.turn_id)
    return CanaryStatus(
        failed_checks=failed,
        missing_echo=missing_echo,
        missing_output=missing_output,
        sampled_chars=sampled_chars,
        last_failed_turn=last_turn,
        recent_turn_ids=tuple(turn_ids),
    )


@omp.hook("before_agent_start", phase=omp.HookPhase.OBSERVE)
async def arm_canary(event: omp.BeforeAgentStartEvent, ctx: omp.Context) -> None:
    """Arm only the next committed turn when the caller requested a probe."""

    del ctx
    global _probe_pending
    _probe_pending = _requested(event.text)


@omp.hook("turn_end", phase=omp.HookPhase.OBSERVE)
async def check_canary(event: omp.TurnEndEvent, ctx: omp.Context) -> None:
    """Sample settled assistant previews and record a requested missing echo."""

    global _probe_pending
    if not _probe_pending:
        return
    _probe_pending = False

    assistant_ids = {
        item.item_id for item in event.items if item.role == "assistant"
    }
    view = await omp.context.view()
    samples = (
        message.preview
        for message in view.messages
        if message.id in assistant_ids and message.role == "assistant"
    )
    expected = _expected_echo(ctx.session)
    found, sampled_chars, assistant_items = _check_samples(samples, expected)
    outcome = "pass" if found else "fail"
    _checks.add(1, outcome=outcome)
    if found:
        return

    reason = "echo_missing" if assistant_items else "assistant_output_missing"
    _failures.add(1, reason=reason)
    omp.journal.append(
        CanaryCheck(
            turn_id=event.turn_id,
            turn_index=event.turn_index,
            reason=reason,
            expected_fingerprint=sha256(expected.encode()).hexdigest()[:16],
            sampled_chars=sampled_chars,
            assistant_items=assistant_items,
        ),
        idempotency_key=f"canary-check:{event.turn_id}",
    )


@omp.device("canary_status", family="canary", rev=1, place="host")
async def canary_status(args: CanaryStatusArgs, ctx: omp.Context) -> CanaryStatus:
    """Fold recent durable failed checks without keeping process-local totals."""

    del ctx
    if not 1 <= args.limit <= 100:
        raise ValueError("limit must be between 1 and 100")
    entries = omp.journal.entries(CanaryCheck, limit=args.limit)
    return _fold_checks(
        entry.value for entry in entries if entry.value is not None
    )
