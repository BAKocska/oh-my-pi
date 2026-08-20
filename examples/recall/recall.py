"""Recall settled tool outcomes from structured telemetry truth."""

from __future__ import annotations

import json
import sqlite3
from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from datetime import timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Any

import omp
from omp import telemetry

if TYPE_CHECKING:
    # GAP: documented query symbols are absent from the frozen telemetry module.
    # docs/py/10-telemetry.md:1145-1240
    from omp.telemetry import QueryResult, Row


_Scalar = str | int | float | bool | None
_ALLOWED_FIELDS = frozenset(
    {
        "outcome",
        "status",
        "target",
        "fault_code",
        "interrupted",
        "useless",
        "abort.kind",
    }
)
_ALLOWED_PREFIXES = (
    "payload.",
    "fault.",
    "arg_issue.",
    "abort.",
    "postcondition.",
    "decoded_args.",
)
_MAX_LIMIT = 40
_MAX_CANDIDATES = 512
_MAX_LOOKBACK_HOURS = 24 * 365


@dataclass(frozen=True, slots=True)
class RecallArgs:
    """Select one exact tool revision and typed outcome predicates."""

    tool: str
    rev: str
    where: dict[str, _Scalar] = field(default_factory=dict)
    outcome: str | None = None
    lookback_hours: int = 24 * 30
    limit: int = 12
    terms: str | None = None


@dataclass(frozen=True, slots=True)
class OutcomeRow:
    """Hold one projected settled outcome without model-facing prose."""

    session: str
    turn: int
    tool: str
    rev: str
    outcome: str
    status: str
    fault_code: str | None
    artifact: str | None
    payload: object | None
    fault: object | None


@dataclass(frozen=True, slots=True)
class RecallResults(omp.Payload):
    """Return a bounded table and explicit query completeness metadata."""

    table: str
    total: int
    shown: int
    truncated: bool
    floored: bool
    backfilled: bool


def _validate(args: RecallArgs) -> None:
    if not args.tool or not args.rev.startswith(f"{args.tool}@"):
        raise ValueError("rev must be an exact canonical identity such as edit@hl.3")
    if any(marker in args.rev for marker in "*?["):
        raise ValueError("rev must select one exact (tool, rev) partition")
    if not 1 <= args.lookback_hours <= _MAX_LOOKBACK_HOURS:
        raise ValueError(f"lookback_hours must be between 1 and {_MAX_LOOKBACK_HOURS}")
    if not 1 <= args.limit <= _MAX_LIMIT:
        raise ValueError(f"limit must be between 1 and {_MAX_LIMIT}")
    for path in args.where:
        if path not in _ALLOWED_FIELDS and not path.startswith(_ALLOWED_PREFIXES):
            raise ValueError(f"unsupported structured field path: {path}")


def _predicate_map(args: RecallArgs) -> dict[str, Any]:
    """Build only core-evaluated equality predicates over typed event fields."""
    eq = telemetry.Eq  # type: ignore[attr-defined]  # GAP: docs/py/10:1150
    predicates = {path: eq(value) for path, value in args.where.items()}
    if args.outcome is not None:
        predicates["outcome"] = eq(args.outcome)
    return predicates


async def _query_outcomes(args: RecallArgs) -> tuple[tuple[OutcomeRow, ...], Any]:
    """Cross the missing frozen query seam exactly once."""
    query_type = telemetry.Query  # type: ignore[attr-defined]  # GAP: docs/py/10:1174
    step_type = telemetry.Step  # type: ignore[attr-defined]  # GAP: docs/py/10:1163
    query = query_type(
        match=(
            step_type(
                kinds=(telemetry.Kind.TOOL_CALL,),
                tool=args.tool,
                rev=args.rev,
                where=_predicate_map(args),
            ),
        ),
        scope=telemetry.Scope.PROJECT,
        since=timedelta(hours=args.lookback_hours),
        select=(
            "at_ms",
            "tool",
            "rev",
            "outcome",
            "status",
            "fault_code",
            "artifact",
            "payload",
            "fault",
        ),
        order_by=("-at_ms",),
        limit=min(max(args.limit * 8, args.limit), _MAX_CANDIDATES),
    )
    result = await telemetry.query(query)  # type: ignore[attr-defined]  # GAP: docs/py/10:1197
    return tuple(_outcome_row(row) for row in result.rows), result


def _outcome_row(row: Any) -> OutcomeRow:
    return OutcomeRow(
        session=str(row.session),
        turn=int(row.turn),
        tool=str(row["tool"]),
        rev=str(row["rev"]),
        outcome=str(row["outcome"]),
        status=str(row["status"]),
        fault_code=None if row.get("fault_code") is None else str(row["fault_code"]),
        artifact=None if row.get("artifact") is None else str(row["artifact"]),
        payload=row.get("payload"),
        fault=row.get("fault"),
    )


def _partition(rows: Iterable[OutcomeRow]) -> dict[tuple[str, str], tuple[OutcomeRow, ...]]:
    """Fold outcomes by their durable `(name, rev)` identity."""
    pending: dict[tuple[str, str], list[OutcomeRow]] = {}
    for row in rows:
        pending.setdefault((row.tool, row.rev), []).append(row)
    return {identity: tuple(group) for identity, group in pending.items()}


def _typed_scalars(prefix: str, value: object) -> Iterable[tuple[str, str]]:
    if isinstance(value, Mapping):
        for key, child in value.items():
            path = f"{prefix}.{key}" if prefix else str(key)
            yield from _typed_scalars(path, child)
    elif isinstance(value, (str, int, float, bool)) or value is None:
        yield prefix, json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def _rebuild_index(path: Path, rows: tuple[OutcomeRow, ...]) -> None:
    """Rebuild an explicitly non-authoritative FTS cache of typed scalar fields."""
    with sqlite3.connect(path) as db:
        db.execute("DROP TABLE IF EXISTS typed_outcomes_fts")
        db.execute(
            "CREATE VIRTUAL TABLE typed_outcomes_fts USING fts5("
            "row_key UNINDEXED, field_path, scalar, tokenize='unicode61')"
        )
        db.execute("CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL)")
        db.execute("INSERT OR REPLACE INTO metadata VALUES ('truth', 'false')")
        for index, row in enumerate(rows):
            values: list[tuple[str, object]] = [
                ("tool", row.tool),
                ("rev", row.rev),
                ("outcome", row.outcome),
                ("status", row.status),
                ("fault_code", row.fault_code),
                ("payload", row.payload),
                ("fault", row.fault),
            ]
            for root, value in values:
                db.executemany(
                    "INSERT INTO typed_outcomes_fts(row_key, field_path, scalar) VALUES (?, ?, ?)",
                    ((str(index), field_path, scalar) for field_path, scalar in _typed_scalars(root, value)),
                )
        db.commit()


def _index_filter(path: Path, rows: tuple[OutcomeRow, ...], terms: str) -> tuple[OutcomeRow, ...]:
    """Use FTS only as a secondary filter over the current typed query result."""
    _rebuild_index(path, rows)
    phrase = '"' + terms.replace('"', '""') + '"'
    with sqlite3.connect(path) as db:
        keys = {
            int(row[0])
            for row in db.execute(
                "SELECT DISTINCT row_key FROM typed_outcomes_fts "
                "WHERE typed_outcomes_fts MATCH ?",
                (phrase,),
            )
        }
    return tuple(row for index, row in enumerate(rows) if index in keys)


def _preview(row: OutcomeRow) -> str:
    if row.artifact is not None:
        return row.artifact
    value = row.payload if row.payload is not None else row.fault
    text = json.dumps(value, ensure_ascii=False, sort_keys=True, default=str, separators=(",", ":"))
    return text if len(text) <= 96 else text[:93] + "..."


def _render(rows: tuple[OutcomeRow, ...], limit: int) -> str:
    lines = [
        "| identity | session:turn | outcome | status | typed evidence |",
        "|---|---|---|---|---|",
    ]
    for (tool, rev), partition in _partition(rows).items():
        identity = rev if rev.startswith(f"{tool}@") else f"{tool}@{rev}"
        for row in partition:
            evidence = _preview(row).replace("|", "\\|").replace("\n", " ")
            lines.append(
                f"| {identity} | {row.session}:{row.turn} | {row.outcome} | "
                f"{row.status} | {evidence} |"
            )
            if len(lines) - 2 >= limit:
                return "\n".join(lines)
    return "\n".join(lines)


@omp.device(
    "recall",
    family="structured",
    rev=1,
    place="env",
    summary="Query settled outcomes by exact tool revision and typed fields.",
)
async def recall(args: RecallArgs, ctx: omp.Context) -> RecallResults:
    """Query durable structured outcomes and return a bounded revision-keyed table."""
    del ctx
    _validate(args)
    rows, query_result = await _query_outcomes(args)
    if args.terms and args.terms.strip():
        state = await omp.state_dir()
        rows = _index_filter(state.local_path() / "recall-index.sqlite", rows, args.terms.strip())
    shown = min(len(rows), args.limit)
    return RecallResults(
        table=_render(rows, args.limit),
        total=query_result.total,
        shown=shown,
        truncated=query_result.truncated or len(rows) > shown,
        floored=query_result.floored,
        backfilled=query_result.backfilled,
    )
