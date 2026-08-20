from __future__ import annotations

import asyncio
from contextlib import asynccontextmanager
from dataclasses import dataclass
from typing import Literal

import omp


PhaseName = Literal[
    "OPEN",
    "ARGS_FINALIZED",
    "ADMISSION",
    "ADMITTED",
    "ASSISTANT_ITEM_COMMITTED",
    "EFFECTS_AUTHORIZED",
    "SETTLED",
]
ProbeMode = Literal["cooperative", "guarded_run"]


@dataclass(frozen=True, slots=True)
class CancelProbeArgs:
    """Select the extension-visible cancellation path to hold open."""

    mode: ProbeMode


@dataclass(frozen=True, slots=True)
class CancelProbeResult:
    """Describe an unexpected normal settlement of a cancellation probe."""

    mode: ProbeMode
    detail: str


@dataclass(frozen=True, slots=True)
class PhaseObservation:
    """Record one row of the seven-phase cancellation matrix."""

    phase: PhaseName
    extension_body_reachable: bool
    cancellation: str
    documented_consequence: str
    observed: str


PHASE_MATRIX = (
    PhaseObservation(
        "OPEN",
        False,
        "abandon speculative call",
        "no effect; DATA refuses EffectsNotAuthorized",
        "stub DATA owner refused before dispatch",
    ),
    PhaseObservation(
        "ARGS_FINALIZED",
        False,
        "cancel finalized call",
        "requested args fixed; no effect; DATA refuses EffectsNotAuthorized",
        "stub DATA owner refused before dispatch",
    ),
    PhaseObservation(
        "ADMISSION",
        False,
        "cancel while gates run",
        "no admission receipt or effect; DATA refuses EffectsNotAuthorized",
        "stub DATA owner refused before dispatch",
    ),
    PhaseObservation(
        "ADMITTED",
        False,
        "cancel admitted call",
        "effective args fixed; no effect; DATA refuses EffectsNotAuthorized",
        "stub DATA owner refused before dispatch",
    ),
    PhaseObservation(
        "ASSISTANT_ITEM_COMMITTED",
        False,
        "cancel durable, unauthorized call",
        "assistant item durable; no effect; DATA refuses EffectsNotAuthorized",
        "stub DATA owner refused before dispatch",
    ),
    PhaseObservation(
        "EFFECTS_AUTHORIZED",
        True,
        "cancel device mid-body",
        "effects unknown; CancelledError unwinds cleanup; guards terminate resources",
        "smoke observed CancelledError, finally/async-with cleanup, effects_unknown, and stale Run refusal",
    ),
    PhaseObservation(
        "SETTLED",
        False,
        "cancel after durable outcome",
        "the immutable CallOutcome remains settled",
        "outcome value stayed immutable and abort kinds remained distinct",
    ),
)


@dataclass(slots=True)
class _CleanupTrace:
    entered: bool = False
    exited: bool = False
    finally_ran: bool = False
    saw_cancelled_error: bool = False


@asynccontextmanager
async def _cleanup_scope(trace: _CleanupTrace):
    trace.entered = True
    try:
        yield
    finally:
        trace.exited = True


async def _wait_for_cancellation(
    ctx: omp.Context, trace: _CleanupTrace, started: asyncio.Event
) -> None:
    """Hold one authorized body open and re-raise the documented cancellation type."""

    ctx.checkpoint()
    try:
        async with _cleanup_scope(trace):
            started.set()
            await asyncio.Future()
    except omp.CancelledError:
        trace.saw_cancelled_error = True
        raise
    finally:
        trace.finally_ran = True


@omp.device(
    "cancel_probe",
    family="cancel",
    rev=1,
    place="env",
    summary="Hold an authorized invocation open for cooperative or guarded cancellation.",
)
async def cancel_probe(
    args: CancelProbeArgs, ctx: omp.Context
) -> CancelProbeResult:
    """Expose the two live paths that an external harness cancels after authorization."""

    if args.mode == "cooperative":
        await _wait_for_cancellation(ctx, _CleanupTrace(), asyncio.Event())
        return CancelProbeResult(args.mode, "cancellation was not delivered")

    async with omp.env.sh.session() as session:
        run = await session.run("sleep 600")
        await run.wait()
    return CancelProbeResult(args.mode, "guarded process settled before cancellation")

class _SmokeContext:
    """Supply the one context checkpoint exercised by the no-host smoke."""

    @staticmethod
    def checkpoint() -> None:
        return None




class _PhaseBackend:
    """Minimal DATA owner used only by the re-runnable stub smoke."""

    def __init__(self, *, authorized: bool) -> None:
        self.authorized = authorized

    async def request(self, operation: str, arguments: dict[str, object]) -> object:
        del arguments
        if not self.authorized:
            raise omp.EffectsNotAuthorized("cancel-probe", operation)
        return {"operation": operation}


class _RunBackend:
    """Model owner-side RunGuard termination and generation fencing for smoke."""

    def __init__(self) -> None:
        self.started = asyncio.Event()
        self.live = False
        self.run_id = b"cancel-probe-run"

    def session(self, options: dict[str, object]) -> dict[str, object]:
        del options
        return {"id": b"cancel-probe-session", "cwd": omp.EnvPath("/probe")}

    async def request(self, operation: str, arguments: dict[str, object]) -> object:
        del arguments
        if operation == "omp.env.Session.run":
            self.live = True
            return {"id": self.run_id}
        if operation == "omp.env.Run.wait":
            if not self.live:
                raise omp.StaleGeneration("cancelled Run guard is stale")
            self.started.set()
            try:
                await asyncio.Future()
            except omp.CancelledError:
                self.live = False
                raise
        if operation == "omp.env.Session.close":
            self.live = False
            return None
        raise AssertionError(f"unexpected smoke operation {operation}")


async def _exercise_cancel_dispatch() -> None:
    """Re-observe cooperative dispatch, delayed escalation, and settlement."""

    import json
    import os
    import struct
    import time
    from types import SimpleNamespace

    from omp import _host
    from omp._scope import Scope

    grace_seconds = 0.150
    interrupts: list[tuple[int, float]] = []
    original_grace = _host.CANCEL_GRACE
    original_interrupt = _host._interrupt
    _host.CANCEL_GRACE = SimpleNamespace(seconds=grace_seconds)
    _host._interrupt = lambda thread_id: interrupts.append(
        (thread_id, time.monotonic())
    )

    def dispatch(host: _host.Host, writer: int, invocation: str) -> float:
        raw = json.dumps(
            {
                "kind": "CancelDispatch",
                "body": {"invocation": invocation},
            }
        ).encode()
        os.write(writer, struct.pack("!I", len(raw)) + raw)
        started_at = time.monotonic()
        host.poll()
        return started_at

    try:
        release = asyncio.Event()
        body_started = asyncio.Event()
        cancel_seen = asyncio.Event()

        async def unsettled_body() -> None:
            body_started.set()
            try:
                await asyncio.Future()
            except omp.CancelledError:
                cancel_seen.set()
                await release.wait()

        reader, writer = os.pipe()
        host = _host.Host(reader)
        scope = Scope(
            invocation="cancel-dispatch-unsettled",
            generation=1,
            principal=object(),
            phase=omp.InvocationPhase.OPEN,
        )
        ctx = omp.Context.from_scope(scope)
        callbacks: list[str] = []
        ctx.on_cancel(lambda: callbacks.append("cancelled"))
        task = asyncio.create_task(unsettled_body())
        await body_started.wait()
        host.track_dispatch(scope.invocation, task, scope)
        cancel_started_at = dispatch(host, writer, scope.invocation)
        await asyncio.wait_for(cancel_seen.wait(), timeout=1.0)
        await asyncio.sleep(0)
        assert ctx.cancelled()
        assert callbacks == ["cancelled"]
        assert not interrupts
        await asyncio.sleep(grace_seconds + 0.05)
        assert len(interrupts) == 1
        assert interrupts[0][1] - cancel_started_at >= (
            grace_seconds - 0.01
        )
        release.set()
        await task
        await asyncio.sleep(0)
        os.close(reader)
        os.close(writer)

        settled_started = asyncio.Event()
        settled_cleanup = asyncio.Event()

        async def settling_body() -> None:
            settled_started.set()
            try:
                await asyncio.Future()
            finally:
                settled_cleanup.set()

        reader, writer = os.pipe()
        host = _host.Host(reader)
        scope = Scope(
            invocation="cancel-dispatch-settled",
            generation=1,
            principal=object(),
            phase=omp.InvocationPhase.OPEN,
        )
        task = asyncio.create_task(settling_body())
        await settled_started.wait()
        host.track_dispatch(scope.invocation, task, scope)
        dispatch(host, writer, scope.invocation)
        try:
            await task
        except omp.CancelledError:
            pass
        else:
            raise AssertionError("CancelDispatch did not cancel the task")
        assert settled_cleanup.is_set()
        await asyncio.sleep(grace_seconds + 0.05)
        assert len(interrupts) == 1
        os.close(reader)
        os.close(writer)
    finally:
        _host.CANCEL_GRACE = original_grace
        _host._interrupt = original_interrupt


async def smoke() -> tuple[PhaseObservation, ...]:
    """Exercise every extension-reachable boundary against inert owner stubs."""

    from omp import env

    from omp import limits

    assert omp.CANCEL_GRACE is limits.CANCEL_GRACE
    assert omp.SHUTDOWN_GRACE is limits.SHUTDOWN_GRACE
    assert omp.HEALTH_TIMEOUT is limits.HEALTH_TIMEOUT
    assert omp.MAX_FRAME_BYTES == 67_108_864

    for row in PHASE_MATRIX[:5]:
        env._install_backend(_PhaseBackend(authorized=False), object())
        try:
            await env.fs.stat(omp.EnvPath(f"/{row.phase.lower()}"))
        except omp.EffectsNotAuthorized as error:
            assert error.invocation == "cancel-probe"
            assert error.spec == "omp.env.fs.stat"
        else:
            raise AssertionError(f"DATA did not refuse during {row.phase}")

    await _exercise_cancel_dispatch()

    trace = _CleanupTrace()
    ctx = _SmokeContext()
    started = asyncio.Event()
    task = asyncio.create_task(_wait_for_cancellation(ctx, trace, started))
    await started.wait()
    task.cancel()
    try:
        await task
    except omp.CancelledError as error:
        assert type(error) is omp.CancelledError
    else:
        raise AssertionError("cooperative body swallowed cancellation")
    assert trace == _CleanupTrace(True, True, True, True)

    end_unknown = omp.ToolExecutionEndEvent(
        call_id="unknown",
        target=None,
        outcome=omp.OutcomeKind.ABORTED,
        duration=omp.Duration("1ms"),
        spilled=False,
        artifact=None,
        effects_unknown=True,
    )
    end_clean = omp.ToolExecutionEndEvent(
        call_id="clean",
        target=None,
        outcome=omp.OutcomeKind.FAULTED,
        duration=omp.Duration("1ms"),
        spilled=False,
        artifact=None,
        effects_unknown=False,
    )
    assert end_unknown.effects_unknown and not end_clean.effects_unknown

    backend = _RunBackend()
    env._install_backend(backend, object())
    session = env.sh.session()
    run = await session.run("sleep 600")
    waiter = asyncio.create_task(run.wait())
    await backend.started.wait()
    waiter.cancel()
    try:
        await waiter
    except omp.CancelledError:
        pass
    else:
        raise AssertionError("Run wait swallowed cancellation")
    assert not backend.live
    try:
        await run.wait()
    except omp.StaleGeneration:
        pass
    else:
        raise AssertionError("cancelled Run handle resurrected its resource")

    effects_unknown = omp.Abort.effects_unknown("effect settlement raced cancellation")
    assert effects_unknown.kind == "effects_unknown"
    cancelled = omp.Aborted(omp.Abort.interrupted("cancelled"))
    skipped = omp.Aborted(omp.Abort.skipped("skipped"))
    denial = omp.PolicyDenied("denied", "probe", "decision-1", ())
    assert isinstance(denial, omp.OmpError)
    assert denial.code == "probe"
    policy_denied = omp.Aborted(
        {"reason": "denied"}, omp.AbortKind.POLICY_DENIED, denial
    )
    assert len({cancelled.kind, skipped.kind, policy_denied.kind}) == 3
    assert cancelled.policy is None and skipped.policy is None
    assert policy_denied.policy is denial

    return PHASE_MATRIX
