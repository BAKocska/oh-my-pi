from __future__ import annotations

from dataclasses import dataclass
from typing import Callable
import warnings

import omp
from omp import agents, journal, limits, telemetry, ui
from omp._registry import DeclarationRegistry


@dataclass(frozen=True, slots=True)
class Result:
    limit: str
    value: object
    at_limit: str
    over_limit: str
    typed: bool | None
    names_limit: bool | None
    before_mutation: bool | None


def _raises(
    action: Callable[[], object],
    error_type: type[BaseException],
    name_fragments: tuple[str, ...],
) -> tuple[str, bool, bool]:
    try:
        action()
    except BaseException as error:
        message = str(error).lower()
        return type(error).__name__, isinstance(error, error_type), any(
            fragment.lower() in message for fragment in name_fragments
        )
    return "accepted", False, False


def _declaration_boundary() -> Result:
    registry = DeclarationRegistry()
    declarations = registry._tools
    for index in range(omp.MAX_DECLARATIONS):
        registry._insert(declarations, index, object(), "probe")
    assert len(declarations) == omp.MAX_DECLARATIONS
    before = len(declarations)
    outcome, typed, named = _raises(
        lambda: registry._insert(declarations, before, object(), "probe"),
        omp.DeclarationLimit,
        ("declaration limit", str(omp.MAX_DECLARATIONS)),
    )
    unchanged = len(declarations) == before
    assert typed and named and unchanged
    return Result(
        "omp.MAX_DECLARATIONS",
        omp.MAX_DECLARATIONS,
        "accepted",
        outcome,
        typed,
        named,
        unchanged,
    )


def _telemetry_boundary(
    name: str,
    value: int,
    at: Callable[[], object],
    over: Callable[[], object],
) -> Result:
    at()
    outcome, typed, named = _raises(
        over,
        telemetry.SubscriptionError,
        (name.rsplit(".", 1)[-1].replace("_MAX", "").lower(), str(value)),
    )
    assert typed and named
    return Result(name, value, "accepted", outcome, typed, named, True)


def _tml_boundary(name: str, value: int, at_source: str, over_source: str) -> Result:
    ui.Tml.raw(at_source)
    outcome, typed, named = _raises(
        lambda: ui.Tml.raw(over_source),
        ui.TmlError,
        (name.rsplit(".", 1)[-1].lower(), str(value)),
    )
    assert typed and named
    return Result(name, value, "accepted", outcome, typed, named, True)


class _MetricSink:
    def __init__(self) -> None:
        self.additions: list[tuple[str, int | float, object]] = []

    def add(self, name: str, value: int | float, attrs: object) -> None:
        self.additions.append((name, value, attrs))


def _constant_rows() -> list[Result]:
    unreachable = "host-only; stub dispatch is NotWiredError"
    descriptive = "descriptive constant; enforcement is host-side"
    return [
        Result("omp.MAX_WORKERS", omp.MAX_WORKERS, unreachable, unreachable, None, None, None),
        Result("omp.workers.RESULT_SPILL_BYTES", omp.workers.RESULT_SPILL_BYTES, unreachable, unreachable, None, None, None),
        Result("omp.devices.HARD_SLOT_BUDGET", omp.devices.HARD_SLOT_BUDGET, unreachable, unreachable, None, None, None),
        Result("omp.devices.PER_DEVICE_CAP", omp.devices.PER_DEVICE_CAP, unreachable, unreachable, None, None, None),
        Result("omp.devices.EXTERNAL_SUMMARY_CAP", omp.devices.EXTERNAL_SUMMARY_CAP, unreachable, unreachable, None, None, None),
        Result("omp.BASH_IR_MAX_SOURCE", omp.BASH_IR_MAX_SOURCE, descriptive, descriptive, None, None, None),
        Result("omp.BASH_IR_MAX_NODES", omp.BASH_IR_MAX_NODES, descriptive, descriptive, None, None, None),
        Result("omp.BASH_IR_MAX_DEPTH", omp.BASH_IR_MAX_DEPTH, descriptive, descriptive, None, None, None),
        Result("omp.POLICY_DEADLINE", omp.POLICY_DEADLINE, unreachable, unreachable, None, None, None),
        Result("omp.APPROVAL_DEADLINE", omp.APPROVAL_DEADLINE, unreachable, unreachable, None, None, None),
        Result("omp.VIOLATION_COALESCE", omp.VIOLATION_COALESCE, descriptive, descriptive, None, None, None),
        Result("omp.limits.REENTRANCY_DEPTH", limits.REENTRANCY_DEPTH, unreachable, unreachable, None, None, None),
        Result("omp.limits.INTERACTIVE_CAP", limits.INTERACTIVE_CAP, unreachable, unreachable, None, None, None),
        Result("omp.limits.SETTLE_CONTINUATION_CAP", limits.SETTLE_CONTINUATION_CAP, unreachable, unreachable, None, None, None),
        Result("omp.limits.SHUTDOWN_BUDGET", limits.SHUTDOWN_BUDGET, unreachable, unreachable, None, None, None),
        Result("omp.limits.OBSERVE_CAP", limits.OBSERVE_CAP, unreachable, unreachable, None, None, None),
        Result("omp.limits.MODIFY_ROUNDS", limits.MODIFY_ROUNDS, descriptive, descriptive, None, None, None),
        Result("omp.journal.MAX_INLINE_BYTES", journal.MAX_INLINE_BYTES, unreachable, unreachable, None, None, None),
        Result("omp.journal.MAX_ENTRY_BYTES", journal.MAX_ENTRY_BYTES, unreachable, unreachable, None, None, None),
        Result("omp.journal.MAX_LABEL_BYTES", journal.MAX_LABEL_BYTES, unreachable, unreachable, None, None, None),
        Result("omp.journal.MAX_ATOMIC_ENTRIES", journal.MAX_ATOMIC_ENTRIES, unreachable, unreachable, None, None, None),
        Result("omp.agents.DEFAULT_MAX_DEPTH", agents.DEFAULT_MAX_DEPTH, unreachable, unreachable, None, None, None),
        Result("omp.agents.DEFAULT_MAX_CONCURRENCY", agents.DEFAULT_MAX_CONCURRENCY, unreachable, unreachable, None, None, None),
        Result("omp.agents.DEFAULT_CONTINUATION_CAP", agents.DEFAULT_CONTINUATION_CAP, unreachable, unreachable, None, None, None),
        Result("omp.agents.STEER_GRACE", agents.STEER_GRACE, unreachable, unreachable, None, None, None),
        Result("omp.agents.MIN_SCHEDULE_INTERVAL", agents.MIN_SCHEDULE_INTERVAL, unreachable, unreachable, None, None, None),
        Result("omp.agents.MAILBOX_CAPACITY", agents.MAILBOX_CAPACITY, unreachable, unreachable, None, None, None),
        Result("omp.agents.MAX_BACKFILL", agents.MAX_BACKFILL, unreachable, unreachable, None, None, None),
        Result("omp.agents.EMPTY_OUTPUT_RETRY_CAP", agents.EMPTY_OUTPUT_RETRY_CAP, unreachable, unreachable, None, None, None),
        Result("omp.telemetry.QUEUE_DEFAULT", telemetry.QUEUE_DEFAULT, descriptive, descriptive, None, None, None),
        Result("omp.telemetry.METRIC_PREFIX", telemetry.METRIC_PREFIX, descriptive, descriptive, None, None, None),
        Result("omp.telemetry.DEFAULT_MAX_BYTES", telemetry.DEFAULT_MAX_BYTES, descriptive, descriptive, None, None, None),
        Result("omp.telemetry.DEFAULT_MAX_LINES", telemetry.DEFAULT_MAX_LINES, descriptive, descriptive, None, None, None),
        Result("omp.telemetry.DEFAULT_MAX_COLUMN", telemetry.DEFAULT_MAX_COLUMN, descriptive, descriptive, None, None, None),
        Result("omp.telemetry.SPILL_BYTES", telemetry.SPILL_BYTES, descriptive, descriptive, None, None, None),
        Result("omp.telemetry.SPILL_LINES", telemetry.SPILL_LINES, descriptive, descriptive, None, None, None),
        Result("omp.telemetry.SPILL_COLUMN", telemetry.SPILL_COLUMN, descriptive, descriptive, None, None, None),
        Result("omp.SPILL_INLINE_LIMIT", omp.SPILL_INLINE_LIMIT, descriptive, descriptive, None, None, None),
        Result("omp.MAX_FRAME_BYTES", omp.MAX_FRAME_BYTES, unreachable, unreachable, None, None, None),
        Result("omp.MAX_PENDING_EFFECTS", omp.MAX_PENDING_EFFECTS, unreachable, unreachable, None, None, None),
        Result("omp.MAX_HOST_CHILDREN", omp.MAX_HOST_CHILDREN, unreachable, unreachable, None, None, None),
        Result("omp.DOCS_TOTAL_BUDGET", omp.DOCS_TOTAL_BUDGET, unreachable, unreachable, None, None, None),
        Result("omp.ui.limits.SLOT_MAX_PER_EXTENSION", ui.limits.SLOT_MAX_PER_EXTENSION, unreachable, unreachable, None, None, None),
        Result("omp.ui.limits.NOTIFY_PER_TURN", ui.limits.NOTIFY_PER_TURN, unreachable, unreachable, None, None, None),
        Result("omp.ui.limits.COMPLETION_DEADLINE", ui.limits.COMPLETION_DEADLINE, unreachable, unreachable, None, None, None),
        Result("omp.ui.limits.RENDER_DEADLINE", ui.limits.RENDER_DEADLINE, unreachable, unreachable, None, None, None),
        Result("omp.ui.limits.OVERLAY_MAX_CONCURRENT", ui.limits.OVERLAY_MAX_CONCURRENT, unreachable, unreachable, None, None, None),
        Result("omp.ui.limits.WATCH_DEBOUNCE", ui.limits.WATCH_DEBOUNCE, unreachable, unreachable, None, None, None),
    ]


def smoke() -> tuple[Result, ...]:
    """Exercise every locally reachable boundary and inventory every other ceiling."""
    assert omp.MAX_DECLARATIONS == 256
    assert omp.MAX_WORKERS == 8
    assert omp.workers.RESULT_SPILL_BYTES == 262_144
    assert (omp.devices.HARD_SLOT_BUDGET, omp.devices.PER_DEVICE_CAP, omp.devices.EXTERNAL_SUMMARY_CAP) == (8, 10_000, 200)
    assert (omp.BASH_IR_MAX_SOURCE, omp.BASH_IR_MAX_NODES, omp.BASH_IR_MAX_DEPTH) == (262_144, 50_000, 128)
    assert (journal.MAX_INLINE_BYTES, journal.MAX_ENTRY_BYTES, journal.MAX_LABEL_BYTES, journal.MAX_ATOMIC_ENTRIES) == (65_536, 16_777_216, 256, 1_024)
    assert (ui.limits.TML_MAX_BYTES, ui.limits.TML_MAX_DEPTH) == (262_144, 64)
    assert (telemetry.MAX_INSTRUMENTS, telemetry.MAX_CARDINALITY) == (256, 1_024)
    assert (
        telemetry.DEFAULT_MAX_BYTES,
        telemetry.DEFAULT_MAX_LINES,
        telemetry.DEFAULT_MAX_COLUMN,
    ) == (51_200, 3_000, 512)
    assert (
        telemetry.SPILL_BYTES,
        telemetry.SPILL_LINES,
        telemetry.SPILL_COLUMN,
    ) == (51_200, 3_000, 512)
    assert omp.MAX_FRAME_BYTES == 67_108_864

    rows = [_declaration_boundary()]
    rows.append(
        _telemetry_boundary(
            "omp.telemetry.QUEUE_MAX",
            65_536,
            lambda: telemetry._subscribe((telemetry.Kind.TURN_END,), queue=65_536),
            lambda: telemetry._subscribe((telemetry.Kind.TURN_END,), queue=65_537),
        )
    )
    rows.append(
        _telemetry_boundary(
            "omp.telemetry.BATCH_MAX",
            telemetry.BATCH_MAX,
            lambda: telemetry._subscribe((telemetry.Kind.TURN_END,), batch=telemetry.BATCH_MAX),
            lambda: telemetry._subscribe((telemetry.Kind.TURN_END,), batch=telemetry.BATCH_MAX + 1),
        )
    )
    rows.append(
        _tml_boundary(
            "omp.ui.limits.TML_MAX_BYTES",
            ui.limits.TML_MAX_BYTES,
            "x" * ui.limits.TML_MAX_BYTES,
            "x" * (ui.limits.TML_MAX_BYTES + 1),
        )
    )
    at_depth = "<a>" * ui.limits.TML_MAX_DEPTH + "</a>" * ui.limits.TML_MAX_DEPTH
    over_depth = "<a>" * (ui.limits.TML_MAX_DEPTH + 1) + "</a>" * (ui.limits.TML_MAX_DEPTH + 1)
    rows.append(_tml_boundary("omp.ui.limits.TML_MAX_DEPTH", ui.limits.TML_MAX_DEPTH, at_depth, over_depth))

    # docs/py/10-telemetry.md's 2026-08-20 ruling fixes these quotas at
    # 256 instruments and 1,024 attribute series.
    telemetry._instruments.clear()
    for index in range(telemetry.MAX_INSTRUMENTS):
        telemetry.counter(f"limits_probe_{index}", unit="1", description="limit probe")
    instrument_count = len(telemetry._instruments)
    outcome, typed, named = _raises(
        lambda: telemetry.counter(
            "limits_probe_over", unit="1", description="limit probe"
        ),
        telemetry.SubscriptionError,
        ("instrument", str(telemetry.MAX_INSTRUMENTS)),
    )
    assert instrument_count == telemetry.MAX_INSTRUMENTS
    assert len(telemetry._instruments) == instrument_count
    assert typed and named
    rows.append(
        Result(
            "omp.telemetry.MAX_INSTRUMENTS",
            telemetry.MAX_INSTRUMENTS,
            "accepted",
            outcome,
            typed,
            named,
            True,
        )
    )

    telemetry._instruments.clear()
    counter = telemetry.counter(
        "limits_probe_cardinality", unit="1", description="limit probe"
    )
    sink = _MetricSink()
    telemetry._install_instrument_sink(sink)
    try:
        for index in range(telemetry.MAX_CARDINALITY):
            counter.add(1, series=index)
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            counter.add(1, series=telemetry.MAX_CARDINALITY)
            counter.add(1, series=telemetry.MAX_CARDINALITY + 1)
        assert len(counter._series) == telemetry.MAX_CARDINALITY
        assert sink.additions[-2][2] == {"overflow": "true"}
        assert sink.additions[-1][2] == {"overflow": "true"}
        assert len(caught) == 1
        assert "cardinality" in str(caught[0].message).lower()
    finally:
        telemetry._install_instrument_sink(None)
        telemetry._instruments.clear()
    rows.append(
        Result(
            "omp.telemetry.MAX_CARDINALITY",
            telemetry.MAX_CARDINALITY,
            "accepted",
            "folded into overflow=true",
            False,
            True,
            True,
        )
    )

    rows.extend(_constant_rows())
    assert len({row.limit for row in rows}) == len(rows)
    return tuple(rows)


if __name__ == "__main__":
    for result in smoke():
        print(result)
