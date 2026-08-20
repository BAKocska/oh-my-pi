from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timedelta
from enum import StrEnum

import omp
from omp import Budget, Ok, Payload, PromptCaps


_MAX_LISTED_SESSIONS = 200


class ReportGroup(StrEnum):
    """Dimension used to bucket the usage table."""

    DAY = "day"
    MODEL = "model"
    PROJECT = "project"


@dataclass(frozen=True, slots=True)
class UsageReportArgs:
    """Select the time window, bucket dimension, project, and table bound."""

    group_by: ReportGroup = ReportGroup.MODEL
    days: int = 30
    project: str | None = None
    max_rows: int = 20


@dataclass(frozen=True, slots=True)
class UsageRow:
    """One bounded row of durable receipt usage."""

    label: str
    requests: int
    tokens: int
    cache_read: int
    cost_nanos_usd: int
    estimated: bool


@dataclass(frozen=True, slots=True)
class UsageReportPayload(Payload):
    """Durable usage-report value projected as a bounded text table."""

    group_by: ReportGroup
    days: int
    listed_sessions: int
    listed_sessions_truncated: bool
    matching_sessions: int
    total_requests: int
    total_tokens: int
    total_cost_nanos_usd: int
    total_cost_estimated: bool
    rows: tuple[UsageRow, ...]
    truncated: bool


def _since_ms(days: int) -> int:
    """Return the inclusive millisecond boundary for a rolling day window."""

    return int((datetime.now().astimezone() - timedelta(days=days)).timestamp() * 1_000)


def _today_ms() -> int:
    """Return local midnight in Unix milliseconds."""

    now = datetime.now().astimezone()
    return int(now.replace(hour=0, minute=0, second=0, microsecond=0).timestamp() * 1_000)


def _query(group_by: ReportGroup, since_ms: int, project: str | None) -> omp.UsageQuery:
    """Build the sole durable-receipt query used by the tool and statusline."""

    dimensions: tuple[omp.GroupBy, ...]
    bucket = omp.Bucket.NONE
    if group_by is ReportGroup.DAY:
        dimensions = ()
        bucket = omp.Bucket.DAY
    elif group_by is ReportGroup.MODEL:
        dimensions = (omp.GroupBy.MODEL,)
    else:
        dimensions = (omp.GroupBy.PROJECT,)

    return omp.UsageQuery(
        since_ms=since_ms,
        group_by=dimensions,
        bucket=bucket,
        filter=omp.SessionFilter(
            project=project,
            since_ms=since_ms,
            kind=None,
            limit=_MAX_LISTED_SESSIONS,
        ),
        include_subagents=True,
    )


def _label(bucket: omp.UsageBucket, group_by: ReportGroup) -> str:
    """Render one indexed bucket key without consulting session files."""

    if group_by is ReportGroup.DAY:
        if bucket.start_ms is None:
            return "unknown"
        return datetime.fromtimestamp(bucket.start_ms / 1_000).astimezone().strftime("%Y-%m-%d")
    return bucket.key.get(group_by.value, "unknown") or "unknown"


def _fold_rows(
    report: omp.UsageReport, group_by: ReportGroup, max_rows: int
) -> tuple[tuple[UsageRow, ...], bool]:
    """Fold indexed buckets into a deterministically bounded table."""

    buckets = report.series if group_by is ReportGroup.DAY else report.groups
    ordered = (
        sorted(buckets, key=lambda item: item.start_ms or 0)
        if group_by is ReportGroup.DAY
        else sorted(buckets, key=lambda item: (-item.cost.nanos_usd, _label(item, group_by)))
    )
    rows = tuple(
        UsageRow(
            label=_label(bucket, group_by),
            requests=bucket.requests,
            tokens=bucket.usage.total,
            cache_read=bucket.usage.cache_read,
            cost_nanos_usd=bucket.cost.nanos_usd,
            estimated=bucket.cost.estimated,
        )
        for bucket in ordered[:max_rows]
    )
    return rows, report.truncated or len(ordered) > max_rows


def _table(payload: UsageReportPayload) -> tuple[str, ...]:
    """Render complete table lines for later byte-budget admission."""

    lines = [
        f"usage by {payload.group_by.value} — last {payload.days}d — "
        f"{payload.matching_sessions} sessions",
        f"{'bucket':<24} {'requests':>8} {'tokens':>12} {'cache':>12} {'cost':>12}",
    ]
    for row in payload.rows:
        estimate = "~" if row.estimated else ""
        lines.append(
            f"{row.label[:24]:<24} {row.requests:>8,} {row.tokens:>12,} "
            f"{row.cache_read:>12,} {estimate}${row.cost_nanos_usd / 1_000_000_000:>10,.2f}"
        )
    total_estimate = "~" if payload.total_cost_estimated else ""
    lines.append(
        f"{'TOTAL':<24} {payload.total_requests:>8,} {payload.total_tokens:>12,} "
        f"{'':>12} {total_estimate}${payload.total_cost_nanos_usd / 1_000_000_000:>10,.2f}"
    )
    if payload.truncated:
        lines.append("[additional indexed buckets omitted]")
    return tuple(f"{line}\n" for line in lines)


class UsageReportDevice:
    """Soft device querying the core-maintained sessions receipt index."""

    Payload = UsageReportPayload

    async def __call__(self, args: UsageReportArgs, ctx: omp.Context) -> UsageReportPayload:
        """Query visible sessions and aggregate durable usage receipts."""

        del ctx
        if not 1 <= args.days <= 365:
            raise ValueError("days must be between 1 and 365")
        if not 1 <= args.max_rows <= 50:
            raise ValueError("max_rows must be between 1 and 50")

        since_ms = _since_ms(args.days)
        session_filter = omp.SessionFilter(
            project=args.project,
            since_ms=since_ms,
            kind=None,
            limit=_MAX_LISTED_SESSIONS,
        )
        infos = await omp.sessions.list(session_filter)
        report = await omp.sessions.usage(_query(args.group_by, since_ms, args.project))
        rows, truncated = _fold_rows(report, args.group_by, args.max_rows)
        return UsageReportPayload(
            group_by=args.group_by,
            days=args.days,
            listed_sessions=len(infos),
            listed_sessions_truncated=len(infos) == _MAX_LISTED_SESSIONS,
            matching_sessions=report.sessions,
            total_requests=report.total.requests,
            total_tokens=report.total.usage.total,
            total_cost_nanos_usd=report.total.cost.nanos_usd,
            total_cost_estimated=report.total.cost.estimated,
            rows=rows,
            truncated=truncated,
        )

    def prompt(self, view: object, caps: PromptCaps) -> list[object]:
        """Project whole table rows within the model's exact text budget."""

        out = Budget(caps)
        match view:
            case Ok(payload):
                for line in _table(payload):
                    if not out.push(line):
                        break
                return out.finish()
            case _:
                raise TypeError("usage_report prompt received an unsupported call outcome")


usage_report = omp.device("usage_report", family="usage", rev=1, place="host")(
    UsageReportDevice()
)


async def _refresh_today_status(ctx: omp.Context) -> None:
    """Paint today's durable receipt spend into the shared statusline."""

    report = await omp.sessions.usage(_query(ReportGroup.DAY, _today_ms(), None))
    estimate = "~" if report.total.cost.estimated else ""
    omp.ui.set_status(
        "usage.today",
        omp.ui.tml(
            "<segment fg=secondary>{value}</segment>",
            value=omp.ui.text(
                f"today {estimate}${report.total.cost.nanos_usd / 1_000_000_000:,.2f}"
            ),
        ),
        order=40,
        side=omp.ui.Slot.STATUS_RIGHT,
    )


@omp.hook("extension_activate")
async def paint_usage_activation(payload: object, ctx: omp.Context) -> None:
    """Seed today's durable spend when the extension activates."""

    del payload
    await _refresh_today_status(ctx)


@omp.hook("turn_end")
async def paint_usage_turn(payload: object, ctx: omp.Context) -> None:
    """Refresh today's durable spend after a settled turn receipt."""

    del payload
    await _refresh_today_status(ctx)
