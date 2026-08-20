from __future__ import annotations

from dataclasses import dataclass
from typing import Any

import omp


_VECTOR_DIMENSIONS = 32
_MAX_QUERY_RESULTS = 32
_HASHLIB: Any = None
_MATH: Any = None
_SQLITE3: Any = None


@omp.entry_kind(
    "examples.vector-memory.memory", rev="v.1", display=False, spill=False
)
@dataclass(frozen=True, slots=True)
class MemoryRecorded:
    """Record one durable memory whose vector representation is derived data."""

    text: str
    tags: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class MemoryPutArgs:
    """Describe one memory to append before rebuilding the derived vector index."""

    text: str
    tags: tuple[str, ...] = ()
    idempotency_key: str | None = None


@dataclass(frozen=True, slots=True)
class MemoryPutResult(omp.Payload):
    """Report the durable journal identity of an indexed memory."""

    entry: str


@dataclass(frozen=True, slots=True)
class MemoryQueryArgs:
    """Describe a bounded vector-memory query and optional tag filter."""

    query: str
    limit: int = 8
    tags: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class MemoryHit:
    """Return one similarity-ranked memory with its durable journal identity."""

    entry: str
    text: str
    tags: tuple[str, ...]
    score: float


@dataclass(frozen=True, slots=True)
class MemoryQueryResult(omp.Payload):
    """Return bounded results from the rebuildable worker-owned vector index."""

    hits: tuple[MemoryHit, ...]


@dataclass(frozen=True, slots=True)
class _ReplayMemory:
    session: str
    index: int
    text: str
    tags: tuple[str, ...]


def _boot_vectors() -> None:
    """Load native-adjacent modules only inside the isolated vector worker."""

    global _HASHLIB, _MATH, _SQLITE3
    import hashlib
    import math
    import sqlite3

    _HASHLIB = hashlib
    _MATH = math
    _SQLITE3 = sqlite3


def _runtime() -> tuple[Any, Any, Any]:
    if _SQLITE3 is None:
        _boot_vectors()
    return _HASHLIB, _MATH, _SQLITE3


def _embed(text: str) -> tuple[float, ...]:
    hashlib, math, _ = _runtime()
    vector = [0.0] * _VECTOR_DIMENSIONS
    tokens = text.casefold().split() or [""]
    for token in tokens:
        digest = hashlib.blake2b(token.encode("utf-8"), digest_size=_VECTOR_DIMENSIONS).digest()
        for offset, byte in enumerate(digest):
            vector[offset] += (byte - 127.5) / 127.5
    norm = math.sqrt(sum(value * value for value in vector)) or 1.0
    return tuple(value / norm for value in vector)


def _connect(state: omp.EnvPath) -> Any:
    _, _, sqlite3 = _runtime()
    root = state.local_path()
    root.mkdir(parents=True, exist_ok=True)
    db = sqlite3.connect(root / "vectors.db")
    db.execute(
        "CREATE TABLE IF NOT EXISTS memories ("
        "session TEXT NOT NULL, entry_index INTEGER NOT NULL, text TEXT NOT NULL, "
        "tags TEXT NOT NULL, vector TEXT NOT NULL, "
        "PRIMARY KEY(session, entry_index))"
    )
    return db


def _encode_vector(vector: tuple[float, ...]) -> str:
    import json

    return json.dumps(vector, separators=(",", ":"))


def _decode_vector(encoded: str) -> tuple[float, ...]:
    import json

    return tuple(float(value) for value in json.loads(encoded))


def _encode_tags(tags: tuple[str, ...]) -> str:
    import json

    return json.dumps(tags, ensure_ascii=False, separators=(",", ":"))


def _decode_tags(encoded: str) -> tuple[str, ...]:
    import json

    return tuple(str(value) for value in json.loads(encoded))


def _rebuild_vectors(state: omp.EnvPath, memories: tuple[_ReplayMemory, ...]) -> int:
    """Replace the worker index from a complete host-supplied journal replay."""

    db = _connect(state)
    try:
        db.execute("BEGIN IMMEDIATE")
        db.execute("DELETE FROM memories")
        db.executemany(
            "INSERT INTO memories(session, entry_index, text, tags, vector) "
            "VALUES (?, ?, ?, ?, ?)",
            (
                (
                    memory.session,
                    memory.index,
                    memory.text,
                    _encode_tags(memory.tags),
                    _encode_vector(_embed(memory.text)),
                )
                for memory in memories
            ),
        )
        db.commit()
    except BaseException:
        db.rollback()
        raise
    finally:
        db.close()
    return len(memories)


def _query_vectors(
    state: omp.EnvPath, query: str, limit: int, required_tags: tuple[str, ...]
) -> tuple[MemoryHit, ...]:
    """Embed and search entirely inside the isolated vector worker."""

    query_vector = _embed(query)
    required = frozenset(required_tags)
    scored: list[MemoryHit] = []
    db = _connect(state)
    try:
        rows = db.execute(
            "SELECT session, entry_index, text, tags, vector FROM memories"
        ).fetchall()
    finally:
        db.close()
    for session, entry_index, text, encoded_tags, encoded_vector in rows:
        tags = _decode_tags(encoded_tags)
        if not required.issubset(tags):
            continue
        vector = _decode_vector(encoded_vector)
        score = sum(left * right for left, right in zip(query_vector, vector, strict=True))
        scored.append(
            MemoryHit(
                entry=f"{session}:{entry_index}",
                text=text,
                tags=tags,
                score=round(score, 6),
            )
        )
    scored.sort(key=lambda hit: (-hit.score, hit.entry))
    return tuple(scored[:limit])


omp.workers.declare(
    omp.WorkerSpec(
        name="vectors",
        site=omp.Site.ENV,
        boot=_boot_vectors,
        idle_ttl=omp.Duration("10m"),
        max_concurrency=1,
        max_calls=100_000,
        restart=omp.Restart.ON_FAILURE,
        resources=omp.WorkerResources(memory_bytes=1 << 30, open_files=256),
    )
)


def _journal_replay() -> tuple[_ReplayMemory, ...]:
    replay: list[_ReplayMemory] = []
    for record in omp.journal.entries(MemoryRecorded):
        value = record.value
        if not isinstance(value, MemoryRecorded):
            continue
        replay.append(
            _ReplayMemory(
                session=record.id.session,
                index=record.id.index,
                text=value.text,
                tags=value.tags,
            )
        )
    return tuple(replay)


async def _synchronize_index() -> omp.EnvPath:
    state = await omp.state_dir()
    worker = await omp.workers.get("vectors")
    await worker.call(_rebuild_vectors, state, _journal_replay())
    return state


@omp.hook("extension_activate", phase=omp.HookPhase.OBSERVE)
async def rebuild_vector_index(
    event: omp.ExtensionActivateEvent, ctx: omp.Context
) -> None:
    """Rebuild the derived vector index after activation or worker-host recovery."""

    del event, ctx
    await _synchronize_index()


@omp.device("memory_put", family="vm", rev=1, place="host")
async def memory_put(args: MemoryPutArgs, ctx: omp.Context) -> MemoryPutResult:
    """Append durable journal truth, then ship index rebuilding to the vector worker."""

    del ctx
    text = args.text.strip()
    if not text:
        raise ValueError("memory text must not be empty")
    tags = tuple(sorted({tag.strip() for tag in args.tags if tag.strip()}))
    entry = omp.journal.append(
        MemoryRecorded(text=text, tags=tags),
        idempotency_key=args.idempotency_key,
    )
    await _synchronize_index()
    return MemoryPutResult(entry=str(entry))


@omp.device("memory_query", family="vm", rev=1, place="host")
async def memory_query(args: MemoryQueryArgs, ctx: omp.Context) -> MemoryQueryResult:
    """Replay journal truth, then ship embedding and similarity search to the worker."""

    del ctx
    query = args.query.strip()
    if not query:
        return MemoryQueryResult(hits=())
    state = await _synchronize_index()
    limit = min(max(args.limit, 1), _MAX_QUERY_RESULTS)
    tags = tuple(sorted({tag.strip() for tag in args.tags if tag.strip()}))
    worker = await omp.workers.get("vectors")
    hits = await worker.call(_query_vectors, state, query, limit, tags)
    return MemoryQueryResult(hits=hits)
