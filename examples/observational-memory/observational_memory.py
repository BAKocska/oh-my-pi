from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Iterable, Mapping

import omp


_MAX_DETAIL_CHARS = 2_000


@omp.entry_kind(
    "examples.observational-memory.observation", rev="v.1", display=False, spill=False
)
@dataclass(frozen=True, slots=True)
class ObservationRecorded:
    """One durable, settled tool outcome in the incremental memory ledger."""

    call_id: str
    tool: str
    outcome: str
    detail: str


def _target_name(target: object) -> str:
    server = getattr(target, "server", None)
    tool = getattr(target, "tool", None)
    if server is not None and tool is not None:
        return f"{server}/{tool}"
    return str(getattr(target, "name", type(target).__name__))


def _json_detail(value: Mapping[str, Any] | None) -> str:
    if value is None:
        return "no structured detail"
    rendered = json.dumps(value, sort_keys=True, separators=(",", ":"), default=str)
    if len(rendered) <= _MAX_DETAIL_CHARS:
        return rendered
    return rendered[: _MAX_DETAIL_CHARS - 1] + "…"


def _observation(ev: omp.ToolResultEvent) -> ObservationRecorded:
    outcome = ev.outcome.value
    detail = ev.payload if ev.payload is not None else ev.fault
    if detail is None:
        detail = ev.abort
    return ObservationRecorded(
        call_id=ev.call_id,
        tool=_target_name(ev.target),
        outcome=outcome,
        detail=_json_detail(detail),
    )


def _ledger() -> tuple[ObservationRecorded, ...]:
    records: list[ObservationRecorded] = []
    for entry in omp.journal.entries(ObservationRecorded):
        value = entry.value
        if isinstance(value, ObservationRecorded):
            records.append(value)
    return tuple(records)


def _summary_text(
    observations: Iterable[ObservationRecorded], previous_summary: str | None = None
) -> tuple[str, int]:
    records = tuple(observations)
    lines = [
        f"- [{record.outcome}] {record.tool}: {record.detail}" for record in records
    ]
    sections = [previous_summary.rstrip()] if previous_summary else []
    sections.append("# Tool observations\n" + "\n".join(lines))
    return "\n\n".join(sections), len(records)


def _first_kept_is_valid(ev: omp.CompactionEvent) -> bool:
    valid_ids = {ref.id for ref in (*ev.to_summarize, *ev.to_retain)}
    return ev.suggested_first_kept in valid_ids


@omp.hook("tool_result", phase=omp.HookPhase.OBSERVE)
async def record_outcome(ev: omp.ToolResultEvent, ctx: omp.Context) -> None:
    """Append each settled tool outcome to the typed journal ledger."""

    del ctx
    omp.journal.append(_observation(ev))


@omp.hook("compaction")
async def supply_fold(
    ev: omp.CompactionEvent, ctx: omp.Context
) -> omp.CompactionVerdict | None:
    """Supply a validated, deterministic ledger summary for the LOCAL tier."""

    del ctx
    if ev.tier is not omp.CompactionTier.LOCAL:
        return None
    observations = _ledger()
    if not observations or not _first_kept_is_valid(ev):
        return None
    summary, count = _summary_text(observations, ev.previous_summary)
    return omp.CustomSummary(
        summary=summary,
        first_kept_id=ev.suggested_first_kept,
        short=f"{count} tool observations",
        details={"observation_count": count},
        preserve={"folded_through": ev.preparation_id},
    )


@omp.hook("compaction_done", phase=omp.HookPhase.OBSERVE)
async def invalidate_memory_prompt(
    ev: omp.CompactionOutcome, ctx: omp.Context
) -> None:
    """Invalidate this extension's epochal memory contribution after compaction."""

    del ev, ctx
    await omp.prompts.invalidate("memory")


@omp.hook("agent_settled")
async def request_compaction(
    ev: omp.AgentSettledEvent, ctx: omp.Context
) -> None:
    """Request sanctioned compaction after settlement when the ledger has content."""

    del ev, ctx
    if not _ledger():
        return None
    try:
        await omp.context.compact()
    except omp.CompactionBusy:
        pass
    return None
