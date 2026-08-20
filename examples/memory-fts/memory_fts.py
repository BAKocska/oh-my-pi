from __future__ import annotations

import sqlite3
from dataclasses import dataclass
from enum import StrEnum
from typing import Any

import omp
from omp import PromptContext
from omp import Context, Payload
from omp import (
    Defer,
    ExtensionActivateEvent,
    HookPhase,
    ToolResultEvent,
)
from omp import entry_kind
from omp import hook
from omp import prompt_slot, prompts
from omp import sessions


class MemoryKind(StrEnum):
    """Classify durable memories by how they are used."""

    FACT = "fact"
    PROFILE = "profile"
    STANDING = "standing"


@entry_kind("dev.hermes.memory.entry", rev="v.1")
@dataclass(frozen=True, slots=True)
class MemoryEntry:
    """Store one durable memory in the project-scoped typed log."""

    text: str
    kind: MemoryKind
    confidence: float
    provenance: str


@dataclass(frozen=True, slots=True)
class RememberArgs:
    """Describe a memory to retain durably."""

    text: str
    kind: MemoryKind
    confidence: float
    provenance: str


@dataclass(frozen=True, slots=True)
class Remembered(Payload):
    """Report the durable state entry assigned to a retained memory."""

    entry: str


@dataclass(frozen=True, slots=True)
class SearchArgs:
    """Describe a full-text memory query."""

    query: str
    limit: int = 8


@dataclass(frozen=True, slots=True)
class SearchHit:
    """Identify one full-text match and its durable source."""

    text: str
    kind: str
    source: str


@dataclass(frozen=True, slots=True)
class SearchResults(Payload):
    """Return bounded full-text matches from the rebuildable index."""

    hits: list[SearchHit]


_profile_snapshot: tuple[str, ...] = ()
_standing_snapshot: tuple[str, ...] = ()
_recall_snapshot: tuple[SearchHit, ...] = ()


def _memory_value(record: Any) -> MemoryEntry | None:
    value = getattr(record, "value", None)
    return value if isinstance(value, MemoryEntry) else None


async def _refresh_stable_snapshots() -> None:
    global _profile_snapshot, _standing_snapshot

    records = await omp.state.entries(MemoryEntry, scope=omp.StateScope.PROJECT)
    profiles: list[str] = []
    standing: list[str] = []
    for record in records:
        value = _memory_value(record)
        if value is None or value.confidence < 0.6:
            continue
        if value.kind is MemoryKind.PROFILE:
            profiles.append(value.text)
        elif value.kind is MemoryKind.STANDING:
            standing.append(value.text)
    _profile_snapshot = tuple(profiles)
    _standing_snapshot = tuple(standing)


@hook("extension_activate")
async def _hydrate_prompt_memory(payload: ExtensionActivateEvent, ctx: Context) -> None:
    """Hydrate pure prompt-slot snapshots from the durable project log."""

    del payload, ctx
    await _refresh_stable_snapshots()

@hook("tool_result", phase=HookPhase.OBSERVE)
async def _capture_recall(payload: ToolResultEvent, ctx: Context) -> Defer:
    """Move env-side search results into the host-side volatile prompt snapshot."""

    del ctx
    global _recall_snapshot

    if getattr(payload.target, "name", None) != "memory_search" or payload.payload is None:
        return Defer()
    raw_hits = payload.payload.get("hits", ())
    _recall_snapshot = tuple(
        SearchHit(
            text=str(hit["text"]),
            kind=str(hit["kind"]),
            source=str(hit["source"]),
        )
        for hit in raw_hits
    )
    return Defer()


@omp.device("memory_remember", family="mr", rev=1)
async def memory_remember(args: RememberArgs, ctx: Context) -> Remembered:
    """Append one typed memory to durable project state."""

    del ctx
    entry = MemoryEntry(
        text=args.text,
        kind=args.kind,
        confidence=args.confidence,
        provenance=args.provenance,
    )
    entry_id = await omp.state.append(entry, scope=omp.StateScope.PROJECT)
    await _refresh_stable_snapshots()
    await prompts.invalidate("guidance")
    return Remembered(entry=str(entry_id))


def _ensure_schema(db: sqlite3.Connection) -> None:
    db.execute(
        "CREATE TABLE IF NOT EXISTS documents ("
        "source TEXT PRIMARY KEY, body TEXT NOT NULL, kind TEXT NOT NULL)"
    )
    db.execute(
        "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts "
        "USING fts5(body, kind UNINDEXED, source UNINDEXED, tokenize='trigram')"
    )
    db.commit()


def _insert_document(db: sqlite3.Connection, source: str, body: str, kind: str) -> None:
    inserted = db.execute(
        "INSERT OR IGNORE INTO documents(source, body, kind) VALUES (?, ?, ?)",
        (source, body, kind),
    )
    if inserted.rowcount:
        db.execute(
            "INSERT INTO memory_fts(body, kind, source) VALUES (?, ?, ?)",
            (body, kind, source),
        )


def _session_text(record: Any) -> str | None:
    direct = getattr(record, "text", None)
    if isinstance(direct, str):
        return direct
    value = getattr(record, "value", None)
    for field in ("text", "content"):
        candidate = getattr(value, field, None)
        if isinstance(candidate, str):
            return candidate
    return None


async def _replay_truth(db: sqlite3.Connection) -> None:
    memory_records = await omp.state.entries(MemoryEntry, scope=omp.StateScope.PROJECT)
    for record in memory_records:
        value = _memory_value(record)
        if value is not None:
            _insert_document(
                db,
                source=f"state:{record.id}",
                body=value.text,
                kind=value.kind.value,
            )

    for session in await sessions.list():
        async for record in sessions.journal(
            session.id,
            kinds=("omp.message",),
            live=True,
        ):
            text = _session_text(record)
            if text:
                _insert_document(
                    db,
                    source=f"session:{session.id}:{record.id}",
                    body=text,
                    kind="session",
                )
    db.commit()


@omp.device("memory_search", family="ms", rev=1, place="env")
async def memory_search(args: SearchArgs, ctx: Context) -> SearchResults:
    """Replay durable truth and search its env-colocated rebuildable FTS index."""

    del ctx

    if not args.query.strip() or args.limit <= 0:
        return SearchResults(hits=[])

    state = await omp.state_dir()
    with sqlite3.connect(state.local_path() / "memory.db") as db:
        _ensure_schema(db)
        await _replay_truth(db)
        phrase = '"' + args.query.replace('"', '""') + '"'
        rows = db.execute(
            "SELECT body, kind, source FROM memory_fts "
            "WHERE memory_fts MATCH ? ORDER BY rank LIMIT ?",
            (phrase, min(args.limit, 32)),
        ).fetchall()

    hits = [SearchHit(text=body, kind=kind, source=source) for body, kind, source in rows]
    return SearchResults(hits=hits)


@prompt_slot("guidance", priority=200)
def profile_and_standing(ctx: PromptContext) -> str:
    """Contribute stable memory policy, profile, and standing instructions."""

    del ctx
    blocks = [
        "Durable memory is available through dyn: fetch `docs/memory_search`, then call "
        "`invoke/memory_search`. Consult it before asking the user to repeat a decision; "
        "current instructions always win."
    ]
    if _profile_snapshot:
        blocks.append("User profile:\n" + "\n".join(f"- {item}" for item in _profile_snapshot))
    if _standing_snapshot:
        blocks.append(
            "Standing memory:\n" + "\n".join(f"- {item}" for item in _standing_snapshot)
        )
    return "\n\n".join(blocks)


@prompt_slot("recall", priority=200)
def recalled_memory(ctx: PromptContext) -> str | None:
    """Contribute only the latest query-dependent recall in the volatile band."""

    del ctx
    if not _recall_snapshot:
        return None
    return "Recalled memory:\n" + "\n".join(f"- {hit.text}" for hit in _recall_snapshot)
