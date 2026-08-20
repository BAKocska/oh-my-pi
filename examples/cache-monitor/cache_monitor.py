from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import entry_kind as _entry_kind
from omp import journal as _journal
from omp import telemetry as _telemetry


_REGRESSION_THRESHOLD = 0.10

_regressions = omp.telemetry.counter(
    "cache.regressions",
    unit="{regression}",
    description="Prompt-cache hit-rate regressions.",
)
_hit_rate = omp.telemetry.histogram(
    "cache.hit_rate",
    unit="1",
    description="Prompt-cache hit rate per model request.",
)
_previous_rate: float | None = None


@omp.entry_kind("examples.cache-monitor.cache.turn", rev="v.1")
@dataclass(frozen=True, slots=True)
class CacheTurn:
    """Durable cache-health observation for one settled model request."""

    rate: float
    cache_read: int
    input_tokens: int
    stable_prefix_bytes: int
    changed_slots: tuple[str, ...]
    served_model: str
    regression: bool


@dataclass(frozen=True, slots=True)
class CacheReportArgs:
    """Arguments selecting how many recent cache turns to summarize."""

    limit: int = 20


@dataclass(frozen=True, slots=True)
class CacheReport:
    """Aggregate cache-health report over recent durable observations."""

    turns: int
    average_hit_rate: float
    regressions: int
    cache_read_tokens: int
    input_tokens: int
    minimum_stable_prefix_bytes: int
    changed_slots: tuple[str, ...]


@omp.telemetry(
    kinds=["model_request"],
    scope=omp.telemetry.Scope.TREE,
    queue=4096,
    overflow=omp.telemetry.Overflow.DROP_OLDEST,
    replay=True,
    replay_limit=2048,
)
async def watch_cache(request: omp.telemetry.ModelRequest, ctx: omp.Context) -> None:
    """Record cache fields and notify when hit rate regresses between requests."""

    del ctx
    global _previous_rate

    usage = request.usage
    rate = usage.cache_read / usage.input if usage.input else 0.0
    changed_slots = tuple(request.prompt.changed)
    regression = (
        _previous_rate is not None
        and _previous_rate - rate > _REGRESSION_THRESHOLD
    )

    _hit_rate.record(rate, model=request.served_model)
    if regression:
        cause = (
            f"prompt slots changed: {', '.join(changed_slots)}"
            if changed_slots
            else "provider-side cache eviction"
        )
        _regressions.add(1, cause=cause.split(":", 1)[0])
        omp.ui.notify(
            f"cache hit rate {rate:.0%} — {cause} "
            f"({request.prompt.prefix_stable_bytes:,} stable prefix bytes)",
            level="warning",
        )

    omp.journal.append(
        CacheTurn(
            rate=rate,
            cache_read=usage.cache_read,
            input_tokens=usage.input,
            stable_prefix_bytes=request.prompt.prefix_stable_bytes,
            changed_slots=changed_slots,
            served_model=request.served_model,
            regression=regression,
        ),
        idempotency_key=f"cache-turn:{request.seq}",
    )
    _previous_rate = rate


@omp.device("cache_report", family="cmr", rev=1, place="host")
async def cache_report(args: CacheReportArgs, ctx: omp.Context) -> CacheReport:
    """Fold recent cache-turn entries into a compact health report."""

    del ctx
    if args.limit < 1:
        raise ValueError("limit must be at least 1")

    entries = omp.journal.entries(CacheTurn, limit=args.limit)
    turns = 0
    rate_sum = 0.0
    regressions = 0
    cache_read = 0
    input_tokens = 0
    minimum_stable: int | None = None
    changed: set[str] = set()

    for entry in entries:
        turn = entry.value
        turns += 1
        rate_sum += turn.rate
        regressions += int(turn.regression)
        cache_read += turn.cache_read
        input_tokens += turn.input_tokens
        minimum_stable = (
            turn.stable_prefix_bytes
            if minimum_stable is None
            else min(minimum_stable, turn.stable_prefix_bytes)
        )
        changed.update(turn.changed_slots)

    return CacheReport(
        turns=turns,
        average_hit_rate=rate_sum / turns if turns else 0.0,
        regressions=regressions,
        cache_read_tokens=cache_read,
        input_tokens=input_tokens,
        minimum_stable_prefix_bytes=minimum_stable or 0,
        changed_slots=tuple(sorted(changed)),
    )
