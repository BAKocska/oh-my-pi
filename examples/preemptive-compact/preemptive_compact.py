from __future__ import annotations

from dataclasses import dataclass
from typing import Iterable, Mapping

import omp


_DEFAULT_THRESHOLD = 0.80
_DEFAULT_HYSTERESIS = 0.05
_MAX_LEDGER_ROWS = 8
_MAX_CONTEXT_ROWS = 32
_MAX_PREVIEW_CHARS = 240


@omp.entry_kind(
    "examples.preemptive-compact.ledger", rev="v.1", display=False, spill=False
)
@dataclass(frozen=True, slots=True)
class CompactionLedgerEntry:
    """Record one pressure observation or compaction-hook decision durably."""

    action: str
    turn_id: str
    fraction: float
    threshold: float
    lower_bound: float
    epoch: int
    detail: str


def _fraction(value: object, default: float) -> float:
    if isinstance(value, bool):
        return default
    try:
        parsed = float(value)
    except (TypeError, ValueError):
        return default
    return parsed if 0.0 <= parsed <= 1.0 else default


def _bands(settings: Mapping[str, object]) -> tuple[float, float]:
    threshold = _fraction(settings.get("threshold"), _DEFAULT_THRESHOLD)
    hysteresis = _fraction(settings.get("hysteresis"), _DEFAULT_HYSTERESIS)
    return threshold, max(0.0, threshold - min(hysteresis, threshold))


def _ledger() -> tuple[CompactionLedgerEntry, ...]:
    records: list[CompactionLedgerEntry] = []
    for row in omp.journal.entries(CompactionLedgerEntry):
        if isinstance(row.value, CompactionLedgerEntry):
            records.append(row.value)
    return tuple(records)


def _trigger_ledger(
    records: Iterable[CompactionLedgerEntry],
) -> tuple[CompactionLedgerEntry, ...]:
    return tuple(
        record
        for record in records
        if record.action in {"requesting", "completed"}
    )


def _is_armed(records: Iterable[CompactionLedgerEntry]) -> bool:
    for record in reversed(tuple(records)):
        if record.action == "rearmed":
            return True
        if record.action in {"requesting", "completed", "busy"}:
            return False
    return True


def _record(
    *,
    action: str,
    turn_id: str,
    fraction: float,
    threshold: float,
    lower_bound: float,
    epoch: int,
    detail: str,
) -> None:
    omp.journal.append(
        CompactionLedgerEntry(
            action=action,
            turn_id=turn_id,
            fraction=fraction,
            threshold=threshold,
            lower_bound=lower_bound,
            epoch=epoch,
            detail=detail,
        )
    )


def _summary(
    event: omp.CompactionEvent, records: tuple[CompactionLedgerEntry, ...]
) -> tuple[str, int]:
    sections: list[str] = []
    if event.previous_summary:
        sections.append(event.previous_summary.rstrip())

    context_rows = []
    for ref in event.to_summarize[-_MAX_CONTEXT_ROWS:]:
        preview = ref.preview.strip().replace("\n", " ")[:_MAX_PREVIEW_CHARS]
        if preview:
            context_rows.append(f"- [{ref.kind.value}] {preview}")
    if context_rows:
        sections.append("# Bounded context previews\n" + "\n".join(context_rows))

    ledger_rows = [
        f"- turn={record.turn_id} pressure={record.fraction:.1%} "
        f"threshold={record.threshold:.1%} action={record.action}"
        for record in records[-_MAX_LEDGER_ROWS:]
    ]
    sections.append("# Proactive compaction ledger\n" + "\n".join(ledger_rows))
    return "\n\n".join(sections), len(ledger_rows)


@omp.hook("turn_start", phase=omp.HookPhase.OBSERVE)
async def observe_pressure(event: omp.TurnStartEvent, ctx: omp.Context) -> None:
    """Request LOCAL compaction once when pressure crosses the configured band."""

    usage = await omp.context.usage()
    threshold, lower_bound = _bands(ctx.settings)
    records = _ledger()

    if usage.fraction <= lower_bound:
        _record(
            action="rearmed",
            turn_id=event.turn_id,
            fraction=usage.fraction,
            threshold=threshold,
            lower_bound=lower_bound,
            epoch=usage.compaction_epoch,
            detail="pressure returned below the hysteresis band",
        )
        return

    if usage.fraction < threshold or not _is_armed(records):
        _record(
            action="held",
            turn_id=event.turn_id,
            fraction=usage.fraction,
            threshold=threshold,
            lower_bound=lower_bound,
            epoch=usage.compaction_epoch,
            detail="inside the hysteresis band or already fired",
        )
        return

    _record(
        action="requesting",
        turn_id=event.turn_id,
        fraction=usage.fraction,
        threshold=threshold,
        lower_bound=lower_bound,
        epoch=usage.compaction_epoch,
        detail="threshold crossed; requesting LOCAL compaction",
    )
    try:
        outcome = await omp.context.compact(tier=omp.CompactionTier.LOCAL)
    except omp.CompactionBusy:
        _record(
            action="busy",
            turn_id=event.turn_id,
            fraction=usage.fraction,
            threshold=threshold,
            lower_bound=lower_bound,
            epoch=usage.compaction_epoch,
            detail="another compaction already owns the lane",
        )
        return
    except Exception as error:
        _record(
            action="failed",
            turn_id=event.turn_id,
            fraction=usage.fraction,
            threshold=threshold,
            lower_bound=lower_bound,
            epoch=usage.compaction_epoch,
            detail=type(error).__name__,
        )
        raise

    _record(
        action="completed",
        turn_id=event.turn_id,
        fraction=usage.fraction,
        threshold=threshold,
        lower_bound=lower_bound,
        epoch=outcome.epoch,
        detail=f"tokens {outcome.tokens_before}->{outcome.tokens_after}",
    )


@omp.hook("compaction")
async def offer_ledger_summary(
    event: omp.CompactionEvent, ctx: omp.Context
) -> omp.CompactionVerdict | None:
    """Offer a bounded deterministic summary only for a ledger-backed LOCAL rung."""

    del ctx
    records = _trigger_ledger(_ledger())
    valid_ids = {ref.id for ref in (*event.to_summarize, *event.to_retain)}
    if (
        event.tier is not omp.CompactionTier.LOCAL
        or not records
        or event.suggested_first_kept not in valid_ids
    ):
        _record(
            action="summary_deferred",
            turn_id="",
            fraction=0.0,
            threshold=0.0,
            lower_bound=0.0,
            epoch=event.epoch,
            detail=f"deferred tier={event.tier.value}",
        )
        return None

    summary, ledger_count = _summary(event, records)
    _record(
        action="summary_offered",
        turn_id="",
        fraction=0.0,
        threshold=0.0,
        lower_bound=0.0,
        epoch=event.epoch,
        detail=f"offered {ledger_count} ledger rows",
    )
    return omp.CustomSummary(
        summary=summary,
        first_kept_id=event.suggested_first_kept,
        short=f"proactive compaction ({ledger_count} triggers)",
        details={"trigger_count": ledger_count},
        preserve={"preparation_id": event.preparation_id},
    )
