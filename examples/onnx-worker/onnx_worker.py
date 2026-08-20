from __future__ import annotations

import hashlib as _hashlib
import json as _json
import math as _math
from dataclasses import asdict as _asdict
from dataclasses import dataclass as _dataclass
from typing import Literal as _Literal

import omp


@_dataclass(frozen=True, slots=True)
class InferArgs:
    """Describe one local text-generation or embedding request."""

    task: _Literal["generate", "embed"]
    text: str
    max_tokens: int = 64
    dimensions: int = 16


@_dataclass(frozen=True, slots=True)
class InferResult(omp.Payload):
    """Carry a typed result from the warm local inference session."""

    task: _Literal["generate", "embed"]
    model: str
    text: str | None = None
    embedding: tuple[float, ...] | None = None


class _StubSession:
    __slots__ = ("model",)

    def __init__(self, model: str) -> None:
        self.model = model

    def generate(self, text: str, max_tokens: int) -> str:
        words = text.split()
        if not words:
            return ""
        return " ".join(words[index % len(words)] for index in range(max_tokens))

    def embed(self, text: str, dimensions: int) -> tuple[float, ...]:
        values: list[float] = []
        counter = 0
        while len(values) < dimensions:
            digest = _hashlib.sha256(f"{counter}:{text}".encode()).digest()
            values.extend((byte - 127.5) / 127.5 for byte in digest)
            counter += 1
        vector = values[:dimensions]
        norm = _math.sqrt(sum(value * value for value in vector)) or 1.0
        return tuple(value / norm for value in vector)


_SESSION: _StubSession | None = None


def _load_session() -> None:
    global _SESSION
    _SESSION = _StubSession("onnx-community/stub-small")


def _session() -> _StubSession:
    global _SESSION
    if _SESSION is None:
        _load_session()
    assert _SESSION is not None
    return _SESSION


def _infer(args: InferArgs) -> InferResult:
    session = _session()
    if args.task == "generate":
        if not 1 <= args.max_tokens <= 4096:
            raise ValueError("max_tokens must be between 1 and 4096")
        return InferResult("generate", session.model, text=session.generate(args.text, args.max_tokens))
    if args.task == "embed":
        if not 1 <= args.dimensions <= 1024:
            raise ValueError("dimensions must be between 1 and 1024")
        return InferResult("embed", session.model, embedding=session.embed(args.text, args.dimensions))
    raise ValueError(f"unsupported inference task: {args.task!r}")


def _spill_if_needed(result: InferResult) -> InferResult | omp.Spill:
    encoded = _json.dumps(_asdict(result), separators=(",", ":")).encode()
    if len(encoded) > omp.workers.RESULT_SPILL_BYTES:
        return omp.Spill(encoded, media_type="application/json")
    return result


omp.workers.declare(
    omp.WorkerSpec(
        name="onnx",
        site=omp.Site.LOCAL,
        boot=_load_session,
        idle_ttl=omp.Duration("10m"),
        max_concurrency=2,
        max_calls=100_000,
        restart=omp.Restart.ON_FAILURE,
        resources=omp.WorkerResources(
            memory_bytes=6 << 30,
            cpu_shares=2.0,
            open_files=256,
            wall_clock=omp.Duration("8h"),
        ),
    )
)


@omp.device("local_infer", family="onnx", rev=1, place="worker:onnx")
async def local_infer(args: InferArgs, ctx: omp.Context) -> InferResult | omp.Spill:
    """Run text generation or embedding in the warm isolated worker."""

    del ctx
    return _spill_if_needed(_infer(args))
