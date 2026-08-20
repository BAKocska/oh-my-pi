from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import Any, Callable

import omp


_CURRENT_HOST_GENERATION = 7
_CURRENT_SESSION_GENERATION = 11


@dataclass(frozen=True, slots=True)
class FencingProbeArgs:
    """Run the complete fixed fencing matrix."""


@dataclass(frozen=True, slots=True)
class ProbeRow:
    """One observed boundary outcome."""

    operation: str
    condition: str
    outcome: str
    conformant: bool


@dataclass(frozen=True, slots=True)
class FencingProbeReport:
    """Return every observation and the frozen-surface finding identifiers."""

    rows: tuple[ProbeRow, ...]
    findings: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class _Frame:
    operation: str
    request_id: str
    idempotency_key: str
    host_generation: int
    session_generation: int


@dataclass(frozen=True, slots=True)
class _Path:
    uri: str


class _GenerationAwareStub:
    """Small state owner that rejects stale frames before touching state."""

    def __init__(self) -> None:
        self.recorded: dict[tuple[str, str], Any] = {}
        self.applies: dict[str, int] = {}
        self.journal: list[str] = []
        self.schedules: dict[str, str] = {}
        self.artifacts: dict[str, str] = {}
        self.approvals: dict[str, str] = {}
        self.process_generations = {"server": 2}
        self.worker_generations = {"indexer": 2, "draining": 1, "evicted": 1}
        self.worker_states = {
            "indexer": omp.WorkerState.READY,
            "draining": omp.WorkerState.DRAINING,
            "evicted": omp.WorkerState.EVICTED,
        }
        self.worker_attempts: list[tuple[str, int]] = []
        self.lease_pins: dict[str, omp.env.Revision] = {}
        self.document_heads: dict[str, omp.env.Revision] = {}

    def _admit(self, frame: _Frame) -> tuple[str, str]:
        if frame.host_generation != _CURRENT_HOST_GENERATION:
            raise omp.StaleGeneration("host generation is stale")
        if frame.session_generation != _CURRENT_SESSION_GENERATION:
            raise omp.StaleGeneration("session generation is stale")
        return frame.operation, frame.idempotency_key

    def durable(self, frame: _Frame, value: str) -> Any:
        key = self._admit(frame)
        if key in self.recorded:
            return self.recorded[key]

        self.applies[frame.operation] = self.applies.get(frame.operation, 0) + 1
        if frame.operation == "journal.append_atomic":
            first = len(self.journal)
            self.journal.extend((f"{value}:0", f"{value}:1"))
            result: Any = (f"entry:{first}", f"entry:{first + 1}")
        elif frame.operation == "schedules.upsert":
            self.schedules[value] = f"schedule:{len(self.schedules) + 1}"
            result = self.schedules[value]
        elif frame.operation == "artifacts.adopt":
            result = self.artifacts.setdefault(value, f"artifact://{len(self.artifacts) + 1}")
        elif frame.operation == "policy.decide":
            ticket, decision = value.split("=", 1)
            self.approvals[ticket] = decision
            result = decision
        else:
            raise AssertionError(f"unknown durable operation {frame.operation!r}")
        self.recorded[key] = result
        return result

    def append_many(self, frame: _Frame, values: tuple[str, ...], fail_at: int) -> None:
        self._admit(frame)
        first = len(self.journal)
        prefix = values[:fail_at]
        self.journal.extend(prefix)
        appended = [omp.journal.EntryId("probe", first + offset) for offset in range(len(prefix))]
        raise omp.JournalError("injected append_many failure", appended=appended)

    async def request(self, operation: str, arguments: dict[str, Any]) -> Any:
        if operation == "omp.env.Process.send":
            name = arguments["name"]
            generation = arguments["generation"]
            if self.process_generations.get(name) != generation:
                raise omp.env.PreconditionFailed("process generation is stale")
            raise AssertionError("the stale process request unexpectedly reached apply")

        if operation == "omp.env.docs.Doc.write":
            lease = arguments["lease"]
            expected = self.lease_pins[lease]
            current = self.document_heads[lease]
            if expected != current:
                raise omp.env.Conflict(
                    "document revision is stale", expected=expected, current=current
                )
            raise AssertionError("the stale document write unexpectedly reached apply")

        if operation == "omp.env.Txn.commit":
            operations = arguments["operations"]
            if len(operations) < 2:
                raise AssertionError("partial probe requires at least two operations")
            raise omp.env.Partial(
                "second transaction operation failed",
                committed=("edit-result:0",),
                failed_index=1,
            )

        raise AssertionError(f"unexpected Environment request {operation!r}")

    async def worker_admin(self, action: str, **kwargs: Any) -> Any:
        if action != "info":
            raise AssertionError(f"unexpected worker admin action {action!r}")
        name = kwargs["name"]
        return omp.WorkerInfo(
            name,
            kwargs["generation"],
            self.worker_states[name],
            omp.Site.ENV,
        )

    async def worker_op(
        self,
        name: str,
        generation: int,
        function: Callable[..., Any],
        args: tuple[Any, ...],
        kwargs: dict[str, Any],
    ) -> Any:
        del function, args, kwargs
        self.worker_attempts.append((name, generation))
        if self.worker_generations[name] != generation:
            raise omp.StaleGeneration("worker generation is stale")
        raise AssertionError("the stale worker request unexpectedly reached apply")


def _frame(
    operation: str,
    request: str,
    key: str,
    *,
    host_generation: int = _CURRENT_HOST_GENERATION,
    session_generation: int = _CURRENT_SESSION_GENERATION,
) -> _Frame:
    return _Frame(operation, request, key, host_generation, session_generation)


def _durable_rows(stub: _GenerationAwareStub) -> list[ProbeRow]:
    rows: list[ProbeRow] = []
    cases = (
        ("journal.append_atomic", "atomic-group"),
        ("schedules.upsert", "nightly"),
        ("artifacts.adopt", "sha256:probe"),
        ("policy.decide", "ticket-1=approved"),
    )
    for operation, value in cases:
        before = stub.applies.get(operation, 0)
        first = stub.durable(_frame(operation, "attempt-1", f"replay:{operation}"), value)
        replay = stub.durable(_frame(operation, "attempt-2", f"replay:{operation}"), value)
        one_apply = stub.applies.get(operation, 0) == before + 1
        rows.append(
            ProbeRow(
                operation,
                "same idempotency key, new request id",
                "recorded result; one apply" if first == replay and one_apply else "double-applied",
                first == replay and one_apply,
            )
        )

        for field, host, session in (
            ("stale host_generation", _CURRENT_HOST_GENERATION - 1, _CURRENT_SESSION_GENERATION),
            ("stale session_generation", _CURRENT_HOST_GENERATION, _CURRENT_SESSION_GENERATION - 1),
        ):
            count = stub.applies.get(operation, 0)
            try:
                stub.durable(
                    _frame(
                        operation,
                        f"stale-{field}",
                        f"stale:{field}:{operation}",
                        host_generation=host,
                        session_generation=session,
                    ),
                    value,
                )
            except omp.StaleGeneration:
                refused = stub.applies.get(operation, 0) == count
            else:
                refused = False
            rows.append(
                ProbeRow(
                    operation,
                    field,
                    "StaleGeneration; no apply" if refused else "accepted stale frame",
                    refused,
                )
            )
    return rows


async def run_probe() -> FencingProbeReport:
    """Exercise every matrix row against one generation-aware state owner."""

    stub = _GenerationAwareStub()
    rows = _durable_rows(stub)

    try:
        stub.append_many(
            _frame("journal.append_many", "partial-1", "partial:many"),
            ("a", "b", "c"),
            2,
        )
    except omp.JournalError as error:
        preserved = [entry.index for entry in error.appended] == [2, 3]
    else:
        preserved = False
    rows.append(
        ProbeRow(
            "journal.append_many",
            "failure after durable prefix",
            "JournalError.appended=[2, 3]" if preserved else "prefix lost",
            preserved,
        )
    )

    omp.env._install_backend(stub, None)
    process = omp.env.Process("server", 1)
    try:
        await process.send(b"probe")
    except (omp.env.PreconditionFailed, omp.StaleGeneration) as error:
        process_refused = type(error).__name__
    else:
        process_refused = "accepted"
    rows.append(
        ProbeRow(
            "Process.send",
            "handle generation 1, current generation 2",
            process_refused,
            process_refused in {"PreconditionFailed", "StaleGeneration"},
        )
    )

    omp.workers.install(stub)
    worker = omp.WorkerHandle("indexer", generation=1)
    try:
        await worker.call(lambda: None)
    except omp.StaleGeneration:
        worker_outcome = "StaleGeneration"
        worker_is_stale = True
    except Exception as error:
        worker_outcome = type(error).__name__
        worker_is_stale = False
    else:
        worker_outcome = "accepted"
        worker_is_stale = False
    generation_forwarded = stub.worker_attempts == [("indexer", 1)]
    worker_fenced = worker_is_stale and generation_forwarded
    rows.append(
        ProbeRow(
            "WorkerHandle.call",
            "handle generation 1, current generation 2",
            (
                f"{worker_outcome}; generation=1 forwarded"
                if generation_forwarded
                else f"{worker_outcome}; generation not forwarded"
            ),
            worker_fenced,
        )
    )

    for worker_name, state in (
        ("draining", omp.WorkerState.DRAINING),
        ("evicted", omp.WorkerState.EVICTED),
    ):
        attempts_before = len(stub.worker_attempts)
        lifecycle_handle = omp.WorkerHandle(worker_name, generation=1)
        try:
            await lifecycle_handle.call(lambda: None)
        except omp.WorkerEvicted:
            lifecycle_outcome = "WorkerEvicted; call not dispatched"
            lifecycle_gated = len(stub.worker_attempts) == attempts_before
        else:
            lifecycle_outcome = "accepted unavailable lifecycle state"
            lifecycle_gated = False
        rows.append(
            ProbeRow(
                "WorkerHandle.call",
                f"worker state is {state.value}",
                lifecycle_outcome,
                lifecycle_gated,
            )
        )

    placement_errors = (
        omp.WorkerUnavailable,
        omp.WorkerEvicted,
        omp.ShipError,
        omp.BoundaryError,
    )
    unified_placement_error = all(
        issubclass(error_type, omp.PlacementError)
        for error_type in placement_errors
    )
    try:
        omp.Place.parse("not-a-place")
    except omp.PlacementError as error:
        native_parse_error = type(error) is omp.PlacementError
    else:
        native_parse_error = False
    placement_conformant = unified_placement_error and native_parse_error
    rows.append(
        ProbeRow(
            "placement errors",
            "all placement failures share native omp.PlacementError",
            (
                "single native PlacementError hierarchy"
                if placement_conformant
                else "split PlacementError hierarchy"
            ),
            placement_conformant,
        )
    )

    old_revision = omp.env.Revision(1, b"old")
    current_revision = omp.env.Revision(2, b"current")
    stub.lease_pins["stale-lease"] = old_revision
    stub.document_heads["stale-lease"] = current_revision
    stale_doc = omp.env.Doc("stale-lease", _Path("file:///probe.txt"), old_revision)
    try:
        await stale_doc.write(b"replacement", on_stale=omp.env.OnStale.FAIL)
    except omp.env.Conflict:
        doc_outcome = "Conflict; no apply"
        doc_fenced = True
    else:
        doc_outcome = "accepted stale revision"
        doc_fenced = False
    rows.append(ProbeRow("Doc.write", "stale pinned revision", doc_outcome, doc_fenced))

    transaction = omp.env.docs.transaction(txn_id=b"partial-probe")
    transaction.write(stale_doc, b"first")
    transaction.write(stale_doc, b"second")
    try:
        await transaction.commit()
    except omp.env.Partial as error:
        partial_distinct = (
            not isinstance(error, omp.env.Conflict)
            and error.failed_index == 1
            and tuple(error.committed) == ("edit-result:0",)
        )
    else:
        partial_distinct = False
    rows.append(
        ProbeRow(
            "Txn.commit",
            "second operation fails after first is durable",
            "Partial(committed=1, failed_index=1)" if partial_distinct else "conflated with Conflict",
            partial_distinct,
        )
    )

    findings = tuple(
        row.operation for row in rows if not row.conformant
    )
    return FencingProbeReport(tuple(rows), findings)


@omp.device("fencing_probe", family="conformance", rev=1, place="host")
async def fencing_probe(
    args: FencingProbeArgs, ctx: omp.Context
) -> FencingProbeReport:
    """Run the idempotency, generation, handle, and revision fencing matrix."""

    del args, ctx
    return await run_probe()


async def smoke() -> None:
    report = await run_probe()
    expected_rows = 20
    if len(report.rows) != expected_rows:
        raise AssertionError(f"expected {expected_rows} matrix rows, got {len(report.rows)}")
    unexpected = [row for row in report.rows if not row.conformant]
    if unexpected:
        raise AssertionError(f"unexpected conformance failures: {unexpected!r}")
    if report.findings:
        raise AssertionError(f"unexpected findings: {report.findings!r}")
    print(f"fencing probe: {len(report.rows)} rows; all conformant")


if __name__ == "__main__":
    asyncio.run(smoke())
