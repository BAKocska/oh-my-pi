from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass

import omp
from omp.agents import (
    Budget,
    Isolation,
    RunStatus,
    SubagentResult,
    SubagentSpec,
    completion,
    spawn,
)


@omp.entry_kind("examples.fresh_loop.iteration", rev="v.1")
@dataclass(frozen=True, slots=True)
class FreshLoopIteration:
    """Record one settled fresh child and its stop verdict."""

    loop_id: str
    iteration: int
    child_id: str
    status: str
    verdict: str
    spend_usd: float
    child_spend_usd: float
    classifier_spend_usd: float
    classifier_fallback: bool
    output_url: str


@dataclass(frozen=True, slots=True)
class FreshLoopArgs:
    """Configure a resumable sequence of clean child sessions."""

    prompt: str
    stop: str
    choices: tuple[str, ...] = ()
    resume_key: str | None = None
    max_iterations: int = 8
    max_requests: int = 8
    max_input_tokens: int = 200_000
    max_output_tokens: int = 40_000
    max_usd: float = 5.0
    max_wall: str = "20m"


@dataclass(frozen=True, slots=True)
class FreshLoopResult:
    """Summarize the durable loop position and terminal reason."""

    loop_id: str
    iterations: int
    stopped: bool
    reason: str
    child_id: str | None
    output_url: str | None
    spend_usd: float
    resumed: bool


def _loop_id(args: FreshLoopArgs) -> str:
    if args.resume_key is not None:
        seed = args.resume_key.strip()
        if not seed:
            raise ValueError("resume_key must be non-empty when provided")
    else:
        seed = json.dumps(
            {"prompt": args.prompt, "stop": args.stop, "choices": args.choices},
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        )
    return hashlib.sha256(seed.encode()).hexdigest()[:24]


def _validate(args: FreshLoopArgs) -> None:
    if not args.prompt.strip():
        raise ValueError("prompt must be non-empty")
    if not args.stop.strip():
        raise ValueError("stop must be non-empty")
    if args.max_iterations < 1:
        raise ValueError("max_iterations must be at least one")
    if args.max_requests < 1:
        raise ValueError("max_requests must be at least one")
    if args.max_input_tokens < 1 or args.max_output_tokens < 1:
        raise ValueError("token budgets must be positive")
    if args.max_usd < 0:
        raise ValueError("max_usd must not be negative")
    if not args.max_wall.strip():
        raise ValueError("max_wall must be non-empty")
    omp.Duration(args.max_wall)

    if not args.choices:
        return
    if any(not choice.strip() for choice in args.choices):
        raise ValueError("classifier choices must be non-empty")
    if len(set(args.choices)) != len(args.choices):
        raise ValueError("classifier choices must be unique")
    if args.stop not in args.choices:
        raise ValueError("stop must name one classifier choice")
    if all(choice == args.stop for choice in args.choices):
        raise ValueError("classifier choices need a non-stop fallback")


def _history(loop_id: str) -> tuple[FreshLoopIteration, ...]:
    return tuple(
        entry.value
        for entry in omp.journal.entries(FreshLoopIteration)
        if isinstance(entry.value, FreshLoopIteration) and entry.value.loop_id == loop_id
    )


def _summary(
    loop_id: str,
    records: tuple[FreshLoopIteration, ...],
    reason: str,
    *,
    resumed: bool,
) -> FreshLoopResult:
    last = records[-1] if records else None
    return FreshLoopResult(
        loop_id=loop_id,
        iterations=len(records),
        stopped=reason == "stop_condition_met",
        reason=reason,
        child_id=None if last is None else last.child_id,
        output_url=None if last is None else last.output_url,
        spend_usd=sum(record.spend_usd for record in records),
        resumed=resumed,
    )


def _terminal_reason(record: FreshLoopIteration) -> str | None:
    return {
        "stop": "stop_condition_met",
        "exhausted": "budget_exhausted",
        "failed": "child_failed",
        "cancelled": "child_cancelled",
    }.get(record.verdict)


async def _classify(
    args: FreshLoopArgs,
    result: SubagentResult,
    loop_id: str,
    iteration: int,
) -> tuple[str, float, bool]:
    if not args.choices:
        verdict = "stop" if args.stop in result.text else "continue"
        return verdict, 0.0, False

    fallback = next(choice for choice in args.choices if choice != args.stop)
    answer = await completion(
        {
            "instruction": (
                "Classify whether the stop condition is met from this typed child "
                "result. Return exactly one allowed choice."
            ),
            "stop_choice": args.stop,
            "child_result": {
                "status": result.status.value,
                "text": result.text,
                "data": result.data,
                "output_url": str(result.output_url),
            },
        },
        role="smol",
        choices=args.choices,
        default=fallback,
        labels={"loop": loop_id, "iteration": str(iteration)},
    )
    verdict = "stop" if answer.choice == args.stop else "continue"
    return verdict, answer.usage.cost_usd, answer.fell_back


@omp.tool("fresh_loop", kind="soft", rev=1)
async def fresh_loop(args: FreshLoopArgs, ctx: omp.Context) -> FreshLoopResult:
    """Run a prompt in clean child sessions until its durable stop verdict."""

    del ctx
    _validate(args)
    loop_id = _loop_id(args)
    records = _history(loop_id)
    resumed = bool(records)

    if records:
        reason = _terminal_reason(records[-1])
        if reason is not None:
            return _summary(loop_id, records, reason, resumed=True)
    if len(records) >= args.max_iterations:
        return _summary(loop_id, records, "max_iterations", resumed=resumed)

    for iteration in range(len(records) + 1, args.max_iterations + 1):
        handle = await spawn(
            SubagentSpec(
                task=args.prompt,
                name=f"FreshLoop{iteration}",
                isolation=Isolation.CLEAN,
                max_depth=0,
                budget=Budget(
                    max_requests=args.max_requests,
                    max_input_tokens=args.max_input_tokens,
                    max_output_tokens=args.max_output_tokens,
                    max_usd=args.max_usd,
                    max_wall=omp.Duration(args.max_wall),
                ),
                labels={"loop": loop_id, "iteration": str(iteration)},
            )
        )
        child = await handle.wait()
        classifier_spend = 0.0
        classifier_fallback = False

        if child.status == RunStatus.COMPLETED:
            verdict, classifier_spend, classifier_fallback = await _classify(
                args, child, loop_id, iteration
            )
        elif child.status == RunStatus.EXHAUSTED:
            verdict = "exhausted"
        elif child.status == RunStatus.CANCELLED:
            verdict = "cancelled"
        else:
            verdict = "failed"

        child_spend = child.subtree_usage.cost_usd
        record = FreshLoopIteration(
            loop_id=loop_id,
            iteration=iteration,
            child_id=child.session_id,
            status=child.status.value,
            verdict=verdict,
            spend_usd=child_spend + classifier_spend,
            child_spend_usd=child_spend,
            classifier_spend_usd=classifier_spend,
            classifier_fallback=classifier_fallback,
            output_url=str(child.output_url),
        )
        omp.journal.append(
            record,
            idempotency_key=f"fresh-loop:{loop_id}:{iteration}",
        )
        records = (*records, record)

        reason = _terminal_reason(record)
        if reason is not None:
            return _summary(loop_id, records, reason, resumed=resumed)

    return _summary(loop_id, records, "max_iterations", resumed=resumed)
