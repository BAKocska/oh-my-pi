from __future__ import annotations

import math
import re
import sqlite3
from collections import Counter
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path

import omp

_TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]{2,}")
_SOURCE_SUFFIXES = frozenset(
    {".c", ".cc", ".cpp", ".go", ".java", ".js", ".jsx", ".py", ".rs", ".ts", ".tsx"}
)
_SCHEMA = """
CREATE TABLE IF NOT EXISTS documents(path TEXT PRIMARY KEY, body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS terms(path TEXT NOT NULL, term TEXT NOT NULL, count INTEGER NOT NULL,
                                  PRIMARY KEY(path, term));
CREATE TABLE IF NOT EXISTS edges(left_term TEXT NOT NULL, right_term TEXT NOT NULL, weight INTEGER NOT NULL,
                                  PRIMARY KEY(left_term, right_term));
"""


@dataclass(frozen=True, slots=True)
class CodeMapArgs:
    """Select a seed expression and diffusion budget for the repository map."""

    query: str
    limit: int = 8
    steps: int = 3
    refresh: bool = False


@dataclass(frozen=True, slots=True)
class HeatedFile:
    """One repository path ranked by graph-diffused heat."""

    path: str
    heat: float
    matched_terms: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class CodeMapResult(omp.Payload):
    """Typed heat map returned to the model on explicit request."""

    query: str
    indexed_files: int
    files: tuple[HeatedFile, ...]


@dataclass(frozen=True, slots=True)
class _HeatAnnotation:
    """A renderer-ready, model-invisible heat summary."""

    files: tuple[HeatedFile, ...]


_HEAT_BY_CALL: dict[str, _HeatAnnotation] = {}


def _terms(text: str) -> Counter[str]:
    """Count normalized identifiers in source text."""

    return Counter(token.casefold() for token in _TOKEN.findall(text))


def _connect(db: Path) -> sqlite3.Connection:
    """Open the replaceable index and ensure its schema."""

    db.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(db)
    connection.executescript(_SCHEMA)
    return connection


def _rebuild_index(db_path: Path, documents: Mapping[str, str]) -> int:
    """Replace the co-occurrence index from a complete source snapshot."""

    with _connect(db_path) as db:
        db.execute("DELETE FROM edges")
        db.execute("DELETE FROM terms")
        db.execute("DELETE FROM documents")
        for path, body in sorted(documents.items()):
            db.execute("INSERT INTO documents(path, body) VALUES (?, ?)", (path, body))
        _reindex_documents(db)
    return len(documents)


def _reindex_documents(db: sqlite3.Connection) -> None:
    """Derive term and undirected edge rows from stored source documents."""

    edges: Counter[tuple[str, str]] = Counter()
    for path, body in db.execute("SELECT path, body FROM documents ORDER BY path"):
        counts = _terms(body)
        db.executemany(
            "INSERT INTO terms(path, term, count) VALUES (?, ?, ?)",
            ((path, term, count) for term, count in sorted(counts.items())),
        )
        names = sorted(counts)
        for index, left in enumerate(names):
            for right in names[index + 1 :]:
                edges[left, right] += min(counts[left], counts[right])
    db.executemany(
        "INSERT INTO edges(left_term, right_term, weight) VALUES (?, ?, ?)",
        ((left, right, weight) for (left, right), weight in sorted(edges.items())),
    )


def _upsert_document(db_path: Path, path: str, body: str) -> None:
    """Refresh one edited document and deterministically rebuild derived rows."""

    with _connect(db_path) as db:
        db.execute(
            "INSERT INTO documents(path, body) VALUES (?, ?) "
            "ON CONFLICT(path) DO UPDATE SET body=excluded.body",
            (path, body),
        )
        db.execute("DELETE FROM edges")
        db.execute("DELETE FROM terms")
        _reindex_documents(db)


def _query_index(db_path: Path, query: str, limit: int, steps: int) -> CodeMapResult:
    """Diffuse query heat through the term graph and rank repository paths."""

    limit = max(1, min(limit, 32))
    steps = max(0, min(steps, 8))
    seeds = tuple(sorted(_terms(query)))
    with _connect(db_path) as db:
        indexed_files = int(db.execute("SELECT count(*) FROM documents").fetchone()[0])
        terms = {row[0] for row in db.execute("SELECT DISTINCT term FROM terms")}
        active = {term: 1.0 for term in seeds if term in terms}
        adjacency: dict[str, list[tuple[str, int]]] = {}
        for left, right, weight in db.execute("SELECT left_term, right_term, weight FROM edges"):
            adjacency.setdefault(left, []).append((right, weight))
            adjacency.setdefault(right, []).append((left, weight))
        for _ in range(steps):
            spread: dict[str, float] = {}
            for term, heat in active.items():
                neighbours = adjacency.get(term, ())
                total = sum(weight for _, weight in neighbours)
                if total:
                    for neighbour, weight in neighbours:
                        spread[neighbour] = spread.get(neighbour, 0.0) + heat * weight / total
            active = {
                term: (1.0 if term in seeds else 0.0) + 0.65 * spread.get(term, 0.0)
                for term in terms
                if term in seeds or term in spread
            }
        scores: dict[str, float] = {}
        matched: dict[str, list[str]] = {}
        for path, term, count in db.execute("SELECT path, term, count FROM terms"):
            heat = active.get(term)
            if heat is None:
                continue
            scores[path] = scores.get(path, 0.0) + heat * math.log2(count + 1)
            matched.setdefault(path, []).append(term)
    ranked = sorted(scores, key=lambda path: (-scores[path], path))[:limit]
    return CodeMapResult(
        query=query,
        indexed_files=indexed_files,
        files=tuple(
            HeatedFile(path, round(scores[path], 6), tuple(sorted(matched[path])[:8]))
            for path in ranked
        ),
    )


async def _workspace_documents() -> dict[str, str]:
    """Read bounded source files through the Environment walker and document API."""

    documents: dict[str, str] = {}
    async for entry in omp.env.find.walk(root=omp.EnvPath("."), follow=omp.env.Follow.NEVER):
        path = entry.path
        if Path(path.uri).suffix.casefold() not in _SOURCE_SUFFIXES or (entry.size or 0) > 262_144:
            continue
        try:
            async with await omp.env.docs.open(path) as document:
                documents[path.uri] = await document.read()
        except (omp.env.EnvError, UnicodeDecodeError):
            continue
    return documents


async def _index_path() -> Path:
    """Resolve the local path of the rebuildable index."""

    state = await omp.state_dir()
    return state.local_path() / "grep-heatmap.sqlite"


@omp.tool("code_map", kind="soft", rev=1)
async def code_map(args: CodeMapArgs, ctx: omp.Context) -> CodeMapResult:
    """Return typed graph heat without changing the model's grep result."""

    del ctx
    db_path = await _index_path()
    if args.refresh or not db_path.exists():
        _rebuild_index(db_path, await _workspace_documents())
    return _query_index(db_path, args.query, args.limit, args.steps)


@omp.hook("tool_result", phase=omp.HookPhase.OBSERVE)
async def refresh_after_result(event: omp.ToolResultEvent, ctx: omp.Context) -> None:
    """Refresh edited source and cache heat for settled native grep calls."""

    del ctx
    if event.outcome is not omp.OutcomeKind.OK or not isinstance(event.target, omp.CoreTool):
        return
    db_path = await _index_path()
    if event.target.name == "edit":
        raw_path = event.target.args.get("path")
        if not isinstance(raw_path, str):
            return
        path = omp.EnvPath(raw_path)
        try:
            async with await omp.env.docs.open(path) as document:
                _upsert_document(db_path, path.uri, await document.read())
        except (omp.env.EnvError, UnicodeDecodeError):
            return
    elif event.target.name == "grep":
        query = event.target.args.get("pattern") or event.target.args.get("query")
        if isinstance(query, str) and db_path.exists():
            result = _query_index(db_path, query, 4, 3)
            _HEAT_BY_CALL[event.call_id] = _HeatAnnotation(result.files)


def render_grep_augmentation(view: omp.View, ctx: omp.ui.RenderCtx) -> omp.ui.Tml | None:
    """Render only a cached heat suffix; never inspect or replace verdict payload parts."""

    del ctx
    if view.verdict is None or not isinstance(view.verdict, omp.Ok):
        return None
    annotation = _HEAT_BY_CALL.get(view.call_id)
    if annotation is None or not annotation.files:
        return None
    labels = " · ".join(f"{item.path} {item.heat:.2f}" for item in annotation.files)
    return omp.ui.tml(
        "<row>{icon}<text fg=muted> graph heat: {labels}</text></row>",
        icon=omp.ui.icon("network"),
        labels=labels,
    )
