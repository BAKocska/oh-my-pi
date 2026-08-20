from __future__ import annotations

import hashlib
import json
import math
import re
import sqlite3
from collections.abc import AsyncIterator, Awaitable, Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import omp

_VECTOR_DIMENSIONS = 64
_MAX_FILES = 10_000
_MAX_FILE_BYTES = 512 * 1024
_TOKEN = re.compile(r"[\w-]{2,}", re.UNICODE)
_TEXT_SUFFIXES = frozenset(
    {
        ".adoc",
        ".c",
        ".cc",
        ".cpp",
        ".css",
        ".go",
        ".h",
        ".html",
        ".java",
        ".js",
        ".json",
        ".jsx",
        ".md",
        ".py",
        ".rst",
        ".rs",
        ".sh",
        ".toml",
        ".ts",
        ".tsx",
        ".txt",
        ".xml",
        ".yaml",
        ".yml",
    }
)
_SCHEMA = """
CREATE TABLE IF NOT EXISTS documents (
    path TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    vector TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
    path UNINDEXED,
    body,
    tokenize='unicode61 remove_diacritics 2'
);
"""


@dataclass(frozen=True, slots=True)
class IngestArgs:
    """Select workspace roots and bounds for one durable ingestion job."""

    roots: tuple[str, ...] = (".",)
    max_files: int = 2_000
    max_file_bytes: int = 256 * 1024


@dataclass(frozen=True, slots=True)
class Ingested(omp.Payload):
    """Report the terminal settlement of a detached ingestion job."""

    indexed: int
    roots: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class SearchArgs:
    """Describe one bounded hybrid full-text and vector query."""

    query: str
    limit: int = 8


@dataclass(frozen=True, slots=True)
class SearchHit:
    """Return one hybrid-ranked source document and bounded excerpt."""

    path: str
    score: float
    lexical_score: float
    vector_score: float
    excerpt: str


@dataclass(frozen=True, slots=True)
class SearchResults(omp.Payload):
    """Return bounded hybrid matches from the rebuildable index."""

    query: str
    indexed: int
    hits: tuple[SearchHit, ...]


_DetachedSubmitter = Callable[
    [AsyncIterator[omp.Update[Any] | omp.Done[Ingested]], omp.Context],
    Awaitable[object],
]


async def _missing_detached_submitter(
    frames: AsyncIterator[omp.Update[Any] | omp.Done[Ingested]], ctx: omp.Context
) -> object:
    del frames, ctx
    raise omp.NotWiredError(
        "omp.Detached/omp.JobRef and env-device JobBoard registration are absent"
    )


_detached_submitter: _DetachedSubmitter = _missing_detached_submitter


def _tokens(text: str) -> tuple[str, ...]:
    return tuple(token.casefold() for token in _TOKEN.findall(text))


def _embed(text: str) -> tuple[float, ...]:
    vector = [0.0] * _VECTOR_DIMENSIONS
    for token in _tokens(text):
        digest = hashlib.blake2b(token.encode("utf-8"), digest_size=16).digest()
        slot = int.from_bytes(digest[:8], "little") % _VECTOR_DIMENSIONS
        vector[slot] += 1.0 if digest[8] & 1 else -1.0
    norm = math.sqrt(sum(value * value for value in vector)) or 1.0
    return tuple(value / norm for value in vector)


def _encode_vector(vector: tuple[float, ...]) -> str:
    return json.dumps(vector, separators=(",", ":"))


def _decode_vector(encoded: str) -> tuple[float, ...]:
    return tuple(float(value) for value in json.loads(encoded))


def _connect(path: Path) -> sqlite3.Connection:
    path.parent.mkdir(parents=True, exist_ok=True)
    database = sqlite3.connect(path)
    database.executescript(_SCHEMA)
    return database


def _replace_index(path: Path, documents: tuple[tuple[str, str], ...]) -> int:
    database = _connect(path)
    try:
        database.execute("BEGIN IMMEDIATE")
        database.execute("DELETE FROM documents")
        database.execute("DELETE FROM documents_fts")
        rows = tuple(
            (source, body, _encode_vector(_embed(body))) for source, body in documents
        )
        database.executemany(
            "INSERT INTO documents(path, body, vector) VALUES (?, ?, ?)", rows
        )
        database.executemany(
            "INSERT INTO documents_fts(path, body) VALUES (?, ?)",
            ((source, body) for source, body, _vector in rows),
        )
        database.commit()
    except BaseException:
        database.rollback()
        raise
    finally:
        database.close()
    return len(documents)


def _excerpt(body: str, terms: tuple[str, ...], maximum: int = 240) -> str:
    collapsed = " ".join(body.split())
    folded = collapsed.casefold()
    offsets = [folded.find(term) for term in terms if term and term in folded]
    start = max(0, (min(offsets) if offsets else 0) - maximum // 4)
    piece = collapsed[start : start + maximum]
    if start:
        piece = "…" + piece
    if start + maximum < len(collapsed):
        piece += "…"
    return piece


def _query_index(path: Path, query: str, limit: int) -> SearchResults:
    terms = tuple(dict.fromkeys(_tokens(query)))
    if not terms or not path.exists():
        return SearchResults(query=query, indexed=0, hits=())
    database = _connect(path)
    try:
        indexed = int(database.execute("SELECT count(*) FROM documents").fetchone()[0])
        expression = " OR ".join('"' + term.replace('"', '""') + '"' for term in terms)
        lexical_rows = database.execute(
            "SELECT path FROM documents_fts WHERE documents_fts MATCH ? "
            "ORDER BY bm25(documents_fts) LIMIT 128",
            (expression,),
        ).fetchall()
        lexical = {source: 1.0 / (rank + 1.0) for rank, (source,) in enumerate(lexical_rows)}
        query_vector = _embed(query)
        ranked: list[SearchHit] = []
        for source, body, encoded in database.execute(
            "SELECT path, body, vector FROM documents"
        ):
            vector = _decode_vector(encoded)
            cosine = sum(
                left * right for left, right in zip(query_vector, vector, strict=True)
            )
            vector_score = (cosine + 1.0) / 2.0
            lexical_score = lexical.get(source, 0.0)
            score = 0.6 * lexical_score + 0.4 * vector_score
            if lexical_score or cosine > 0.0:
                ranked.append(
                    SearchHit(
                        path=source,
                        score=round(score, 6),
                        lexical_score=round(lexical_score, 6),
                        vector_score=round(vector_score, 6),
                        excerpt=_excerpt(body, terms),
                    )
                )
    finally:
        database.close()
    ranked.sort(key=lambda hit: (-hit.score, hit.path))
    return SearchResults(query=query, indexed=indexed, hits=tuple(ranked[:limit]))


async def _workspace_documents(
    args: IngestArgs, ctx: omp.Context
) -> AsyncIterator[tuple[str, str]]:
    seen: set[str] = set()
    maximum_files = min(max(args.max_files, 1), _MAX_FILES)
    maximum_bytes = min(max(args.max_file_bytes, 1), _MAX_FILE_BYTES)
    for root_name in args.roots:
        root = omp.EnvPath(root_name)
        async for entry in omp.env.find.walk(root=root, follow=omp.env.Follow.NEVER):
            ctx.checkpoint()
            if len(seen) >= maximum_files:
                return
            path = entry.path
            if (
                entry.kind != "file"
                or (entry.size or 0) > maximum_bytes
                or Path(path.uri).suffix.casefold() not in _TEXT_SUFFIXES
                or path.uri in seen
            ):
                continue
            try:
                async with await omp.env.docs.open(path) as document:
                    body = await document.read()
            except (omp.env.EnvError, UnicodeDecodeError):
                continue
            seen.add(path.uri)
            yield path.uri, body


async def _index_path() -> Path:
    state = await omp.state_dir()
    return state.local_path() / "knowledge.sqlite"


async def _ingestion_frames(
    args: IngestArgs, ctx: omp.Context
) -> AsyncIterator[omp.Update[Any] | omp.Done[Ingested]]:
    roots = tuple(dict.fromkeys(root.strip() for root in args.roots if root.strip()))
    if not roots:
        raise ValueError("at least one ingestion root is required")
    normalized = IngestArgs(
        roots=roots,
        max_files=args.max_files,
        max_file_bytes=args.max_file_bytes,
    )
    yield omp.Update(stage="walking", roots=roots, indexed=0)
    documents: list[tuple[str, str]] = []
    async for source, body in _workspace_documents(normalized, ctx):
        documents.append((source, body))
        if len(documents) % 25 == 0:
            yield omp.Update(stage="walking", roots=roots, indexed=len(documents))
    documents.sort(key=lambda row: row[0])
    yield omp.Update(stage="committing", roots=roots, indexed=len(documents))
    indexed = _replace_index(await _index_path(), tuple(documents))
    yield omp.Done(Ingested(indexed=indexed, roots=roots))


@omp.device("ingest", family="knowledge", rev=1, place="env", tier=omp.Tier.WRITE)
async def ingest(args: IngestArgs, ctx: omp.Context) -> object:
    """Detach bounded workspace ingestion and settle later through the JobBoard."""

    return await _detached_submitter(_ingestion_frames(args, ctx), ctx)


@omp.device("search", family="knowledge", rev=1, place="env", tier=omp.Tier.READ)
async def search(args: SearchArgs, ctx: omp.Context) -> SearchResults:
    """Query the env-colocated rebuildable index with lexical and vector rank."""

    del ctx
    query = args.query.strip()
    if not query:
        return SearchResults(query="", indexed=0, hits=())
    return _query_index(await _index_path(), query, min(max(args.limit, 1), 32))
