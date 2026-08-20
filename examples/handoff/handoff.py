"""Clean-context implementation handoff built only from durable session truth."""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass
from typing import Sequence

import omp
from omp import ui
from omp.agents import Budget, Isolation, SubagentHandle, SubagentSpec


_BRIEF_MAX_BYTES = 262_144
_FACT_MAX_BYTES = 2_048
_OBJECTIVE_MAX_BYTES = 4_096
_MAX_FACTS = 128
_OWN_KINDS = frozenset({"examples.handoff.brief", "examples.handoff.issued"})
_OUTCOME_WORDS = frozenset({"call_outcome", "outcome", "settled", "tool_result"})


@omp.entry_kind("examples.handoff.brief", rev="v.1", spill=True)
@dataclass(frozen=True, slots=True)
class HandoffBrief:
    """Bounded implementation brief folded from durable parent facts."""

    parent_session: str
    objective: str
    decisions: tuple[str, ...]
    settled_outcomes: tuple[str, ...]
    omitted_entries: int


@omp.entry_kind("examples.handoff.issued", rev="v.1", spill=False)
@dataclass(frozen=True, slots=True)
class HandoffIssued:
    """Link one settled parent to the clean child that received its brief."""

    parent_session: str
    child_session: str
    child_transcript_url: str
    brief_entry: str
    brief_artifact_url: str | None


@dataclass(frozen=True, slots=True)
class _Fact:
    kind: str
    entry_id: str
    data: str


def _truncate_utf8(text: str, maximum: int) -> str:
    """Return a UTF-8-safe prefix no larger than ``maximum`` bytes."""

    encoded = text.encode("utf-8")
    if len(encoded) <= maximum:
        return text
    return encoded[:maximum].decode("utf-8", "ignore")


def _fact(entry: omp.JournalEntry[object]) -> str:
    """Encode one bounded fact without interpreting transcript prose."""

    raw = entry.raw.decode("utf-8", "replace")
    return json.dumps(
        asdict(_Fact(entry.kind, str(entry.id), _truncate_utf8(raw, _FACT_MAX_BYTES))),
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    )


def _is_settled_outcome(entry: omp.JournalEntry[object]) -> bool:
    """Recognize core settlement records without scraping message content."""

    normalized = entry.kind.casefold().replace("-", "_").replace(".", "_")
    value_name = type(entry.value).__name__.casefold()
    return entry.kind.startswith("omp.") and (
        any(word in normalized for word in _OUTCOME_WORDS)
        or value_name in {"ok", "faulted", "argsrejected", "aborted", "calloutcome"}
    )


def _encoded(brief: HandoffBrief) -> bytes:
    """Encode a brief exactly as the clean child receives it."""

    return json.dumps(
        asdict(brief), ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")


def _bounded_brief(
    entries: Sequence[omp.JournalEntry[object]], parent_session: str, objective: str
) -> HandoffBrief:
    """Fold readable durable facts into a deterministic hard byte ceiling."""

    decisions: list[str] = []
    outcomes: list[str] = []
    omitted = 0
    for entry in entries:
        if entry.kind in _OWN_KINDS:
            continue
        if _is_settled_outcome(entry):
            target = outcomes
        elif not entry.kind.startswith("omp."):
            target = decisions
        else:
            continue
        if len(decisions) + len(outcomes) >= _MAX_FACTS:
            omitted += 1
            continue
        target.append(_fact(entry))

    brief = HandoffBrief(
        parent_session=parent_session,
        objective=_truncate_utf8(
            objective.strip() or "Implement the durable decisions and settled outcomes.",
            _OBJECTIVE_MAX_BYTES,
        ),
        decisions=tuple(decisions),
        settled_outcomes=tuple(outcomes),
        omitted_entries=omitted,
    )
    while len(_encoded(brief)) > _BRIEF_MAX_BYTES:
        if brief.decisions:
            brief = HandoffBrief(
                brief.parent_session,
                brief.objective,
                brief.decisions[:-1],
                brief.settled_outcomes,
                brief.omitted_entries + 1,
            )
        elif brief.settled_outcomes:
            brief = HandoffBrief(
                brief.parent_session,
                brief.objective,
                brief.decisions,
                brief.settled_outcomes[:-1],
                brief.omitted_entries + 1,
            )
        else:
            raise ValueError("handoff brief metadata exceeds its hard byte ceiling")
    return brief


def _artifact_url(artifact: object) -> str:
    """Extract the documented resolvable URL from a spilled journal artifact."""

    candidate = getattr(artifact, "url", artifact)
    text = str(candidate)
    if not text.startswith("artifact://"):
        raise RuntimeError("spilled handoff brief has no resolvable artifact URL")
    return text


def _child_seed(brief: HandoffBrief, brief_entry: omp.EntryId) -> tuple[str, str | None]:
    """Return the inline brief or its journal-spill reference, and artifact URL."""

    encoded = _encoded(brief)
    if len(encoded) <= omp.journal.MAX_INLINE_BYTES:
        return encoded.decode("utf-8"), None

    rows = omp.journal.entries(HandoffBrief, limit=1)
    row = next((candidate for candidate in rows if candidate.id == brief_entry), None)
    if row is None or row.artifact is None:
        raise RuntimeError("oversized handoff brief did not spill through the journal")
    url = _artifact_url(row.artifact)
    pointer = json.dumps(
        {
            "brief_artifact_url": url,
            "brief_entry": str(brief_entry),
            "parent_session": brief.parent_session,
        },
        separators=(",", ":"),
        sort_keys=True,
    )
    return pointer, url


async def _spawn_child(seed: str, parent_session: str) -> SubagentHandle:
    """Spawn a clean, leaf implementation child under hard subtree ceilings."""

    return await omp.agents.spawn(
        SubagentSpec(
            task=seed,
            name="HandoffImplementer",
            isolation=Isolation.CLEAN,
            max_depth=0,
            background=True,
            budget=Budget(
                max_requests=24,
                max_input_tokens=240_000,
                max_output_tokens=60_000,
                max_usd=12.0,
                max_wall=omp.Duration("45m"),
            ),
            labels={"handoff_parent": parent_session},
        )
    )


@omp.command(
    "handoff",
    description="Start a clean, budgeted implementation child from durable session facts",
    hint="[implementation objective]",
)
async def handoff(inv: ui.Invocation, ctx: omp.Context) -> ui.Consumed:
    """Issue a durable parent-to-child implementation handoff."""

    objective = " ".join(inv.argv)
    brief = _bounded_brief(omp.journal.entries(), ctx.session, objective)
    brief_entry = omp.journal.append(
        brief,
        display=False,
        idempotency_key=f"handoff-brief:{ctx.invocation}",
    )
    seed, artifact_url = _child_seed(brief, brief_entry)
    child = await _spawn_child(seed, ctx.session)
    try:
        omp.journal.append(
            HandoffIssued(
                parent_session=ctx.session,
                child_session=child.session_id,
                child_transcript_url=str(child.transcript_url),
                brief_entry=str(brief_entry),
                brief_artifact_url=artifact_url,
            ),
            idempotency_key=f"handoff-issued:{ctx.invocation}",
        )
    except Exception:
        await child.cancel(reason="parent-to-child handoff link did not land")
        raise

    return ui.Consumed(
        ui.text(
            f"Implementation handoff issued to {child.name}: {child.transcript_url}"
        )
    )
