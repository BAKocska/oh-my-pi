from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timezone
from decimal import Decimal

import omp
from omp.provider import (
    Api,
    ManagementSpec,
    Operation,
    ProviderSpec,
    RouteSpec,
    Transport,
    UsageQuery as ProviderUsageQuery,
    UsageReport as ProviderUsageReport,
    UsageScope,
    UsageUnit,
    UsageWindow,
)

_PROVIDER_ID = "synthetic-provider"
_REQUEST_LIMIT = 1_000
_TOKEN_LIMIT = 1_000_000

_PROVIDER_SPEC = ProviderSpec(
    id=_PROVIDER_ID,
    name="Synthetic Provider Quota",
    routes=(
        RouteSpec(
            id="usage",
            base_url="local://synthetic-provider",
            api=Api.LOCAL,
            transport=Transport.LOCAL,
        ),
    ),
    management=ManagementSpec(
        operations=frozenset({Operation.USAGE}),
        principal_quota=False,
    ),
)


@omp.provider(_PROVIDER_SPEC)
class SyntheticQuotaProvider:
    """Declare a usage-only provider whose quota projection comes from durable receipts."""


@dataclass(frozen=True, slots=True)
class _QuotaRow:
    label: str
    used: int
    limit: int
    unit: UsageUnit

    @property
    def remaining(self) -> int:
        return max(self.limit - self.used, 0)

    @property
    def remaining_fraction(self) -> Decimal:
        if self.limit <= 0:
            return Decimal(0)
        return Decimal(self.remaining) / Decimal(self.limit)

    @property
    def used_fraction(self) -> Decimal:
        if self.limit <= 0:
            return Decimal(1)
        return Decimal(self.used) / Decimal(self.limit)


def _billing_bounds(now: datetime | None = None) -> tuple[int, int]:
    current = now or datetime.now(timezone.utc)
    current = current.astimezone(timezone.utc)
    start = current.replace(day=1, hour=0, minute=0, second=0, microsecond=0)
    if start.month == 12:
        end = start.replace(year=start.year + 1, month=1)
    else:
        end = start.replace(month=start.month + 1)
    return int(start.timestamp() * 1_000), int(end.timestamp() * 1_000)


def _usage_query(since_ms: int) -> omp.UsageQuery:
    return omp.UsageQuery(
        since_ms=since_ms,
        group_by=(omp.GroupBy.PROVIDER,),
        bucket=omp.Bucket.NONE,
        filter=omp.SessionFilter(since_ms=since_ms, kind=None, limit=10_000),
        include_subagents=True,
    )


def _quota_rows(report: omp.UsageReport) -> tuple[_QuotaRow, ...]:
    provider = next(
        (bucket for bucket in report.groups if bucket.key.get("provider") == _PROVIDER_ID),
        None,
    )
    requests = provider.requests if provider is not None else 0
    tokens = provider.usage.total if provider is not None else 0
    return (
        _QuotaRow("Monthly requests", requests, _REQUEST_LIMIT, UsageUnit.REQUESTS),
        _QuotaRow("Monthly tokens", tokens, _TOKEN_LIMIT, UsageUnit.TOKENS),
    )


def _severity(remaining: Decimal) -> omp.ui.Token:
    if remaining <= Decimal("0.10"):
        return omp.ui.Token.ERR
    if remaining <= Decimal("0.25"):
        return omp.ui.Token.WARN
    return omp.ui.Token.OK


def _render_segment(rows: tuple[_QuotaRow, ...]) -> omp.ui.Tml:
    remaining = min((row.remaining_fraction for row in rows), default=Decimal(0))
    return omp.ui.tml(
        "<segment fg={tone}>{icon}{value}</segment>",
        tone=_severity(remaining),
        icon=omp.ui.icon("gauge"),
        value=omp.ui.text(f"{remaining:.0%} quota"),
    )


def _render_table(rows: tuple[_QuotaRow, ...], resets_at_ms: int) -> omp.ui.Tml:
    table_rows = [
        omp.ui.tml(
            "<tr><td bold>Window</td><td bold>Used</td><td bold>Limit</td>"
            "<td bold>Remaining</td></tr>"
        )
    ]
    for row in rows:
        table_rows.append(
            omp.ui.tml(
                "<tr><td>{label}</td><td>{used}</td><td>{limit}</td>"
                "<td fg={tone}>{remaining}</td></tr>",
                label=omp.ui.text(row.label),
                used=omp.ui.text(f"{row.used:,} {row.unit.value}"),
                limit=omp.ui.text(f"{row.limit:,}"),
                tone=_severity(row.remaining_fraction),
                remaining=omp.ui.text(f"{row.remaining:,} ({row.remaining_fraction:.0%})"),
            )
        )
    reset = datetime.fromtimestamp(resets_at_ms / 1_000, timezone.utc).strftime(
        "%Y-%m-%d %H:%M UTC"
    )
    return omp.ui.tml(
        "<box title='quota' border=round><table gap=2>{rows}</table>"
        "<text fg=muted>Resets {reset}</text></box>",
        rows=table_rows,
        reset=omp.ui.text(reset),
    )


async def _read_quota() -> tuple[tuple[_QuotaRow, ...], int]:
    since_ms, resets_at_ms = _billing_bounds()
    report = await omp.sessions.usage(_usage_query(since_ms))
    return _quota_rows(report), resets_at_ms


async def _refresh_status(ctx: omp.Context) -> None:
    del ctx
    rows, _ = await _read_quota()
    omp.ui.set_status(
        "quota.remaining",
        _render_segment(rows),
        order=50,
        side=omp.ui.Slot.STATUS_RIGHT,
    )


@omp.hook("provider_usage", provider=_PROVIDER_ID)
async def supply_quota(
    query: ProviderUsageQuery, ctx: omp.Context
) -> ProviderUsageReport | None:
    """Project the provider's monthly quota windows from durable session receipts."""

    del ctx
    if query.provider != _PROVIDER_ID:
        return None
    rows, resets_at_ms = await _read_quota()
    windows = ()
    if query.scope is not UsageScope.RATE_LIMIT:
        windows = tuple(
            UsageWindow(
                id=row.label.lower().replace(" ", "_"),
                used=row.used,
                limit=row.limit,
                fraction=row.used_fraction,
                resets_at_ms=resets_at_ms,
                unit=row.unit,
            )
            for row in rows
        )
    return ProviderUsageReport(windows=windows, plan="synthetic-monthly")


@omp.ui.command("quota", description="Show the full synthetic-provider quota table.")
async def show_quota(
    invocation: omp.ui.Invocation, ctx: omp.Context
) -> omp.ui.Consumed:
    """Consume `/quota` with the complete durable-receipt quota table."""

    del invocation, ctx
    rows, resets_at_ms = await _read_quota()
    return omp.ui.Consumed(_render_table(rows, resets_at_ms))


@omp.hook("extension_activate")
async def paint_quota_activation(payload: object, ctx: omp.Context) -> None:
    """Seed the remaining-quota status segment when the extension activates."""

    del payload
    await _refresh_status(ctx)


@omp.hook("turn_end")
async def paint_quota_turn(payload: object, ctx: omp.Context) -> None:
    """Refresh remaining quota after the settled turn receipt is durable."""

    del payload
    await _refresh_status(ctx)
