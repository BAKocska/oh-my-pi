"""Prompt-history completions rendered by the harness as popup and ghost text."""

from __future__ import annotations

import json
from collections.abc import Iterable, Mapping, Sequence
from typing import Any

import omp
from omp import sessions, ui


_MAX_RESULTS = 20
_MAX_SESSIONS = 8
_MAX_ROWS_PER_SESSION = 64


def _member(value: object, name: str) -> object | None:
    if isinstance(value, Mapping):
        return value.get(name)
    return getattr(value, name, None)


def _text_content(content: object) -> str | None:
    if isinstance(content, str):
        return content
    if not isinstance(content, Sequence) or isinstance(content, (bytes, bytearray)):
        return None
    parts: list[str] = []
    for part in content:
        text = _member(part, "text")
        if isinstance(text, str):
            parts.append(text)
    joined = "".join(parts).strip()
    return joined or None


def _history_prompt(row: object) -> str | None:
    value = _member(row, "value")
    if value is None:
        value = row
    role = _member(value, "role")
    role = getattr(role, "value", role)
    if not isinstance(role, str) or role.casefold() != "user":
        return None
    for name in ("content", "text"):
        text = _text_content(_member(value, name))
        if text:
            return text.strip()
    return None


def _score(text: str, query: str) -> int | None:
    candidate = text.casefold()
    needle = query.strip().casefold()
    if not needle:
        return 0
    if candidate.startswith(needle):
        return 4_000
    boundary = candidate.find(f" {needle}")
    if boundary >= 0:
        return 3_000 - boundary
    contained = candidate.find(needle)
    if contained >= 0:
        return 2_000 - contained
    positions = iter(candidate)
    if all(any(char == wanted for char in positions) for wanted in needle):
        return 1_000
    return None


def _rank_candidates(
    query: str,
    history_rows: Iterable[object],
    snippets: Iterable[str],
    *,
    limit: int = _MAX_RESULTS,
) -> list[ui.CompletionItem]:
    ranked: list[tuple[int, int, str, str]] = []
    seen: set[str] = set()
    sources = (
        ((prompt, "Prompt history") for row in history_rows if (prompt := _history_prompt(row))),
        ((snippet.strip(), "Configured snippet") for snippet in snippets if snippet.strip()),
    )
    position = 0
    for source in sources:
        for text, description in source:
            identity = text.casefold()
            if identity in seen:
                continue
            seen.add(identity)
            score = _score(text, query)
            if score is not None:
                ranked.append((score, -position, text, description))
            position += 1
    ranked.sort(reverse=True)

    items: list[ui.CompletionItem] = []
    folded_query = query.casefold()
    for score, _, text, description in ranked[: max(0, limit)]:
        hint = text[len(query) :] if text.casefold().startswith(folded_query) else None
        first_line = text.splitlines()[0]
        label = first_line if len(first_line) <= 72 else first_line[:69] + "..."
        items.append(
            ui.CompletionItem(
                insert=text,
                label=label,
                desc=description,
                hint=hint or None,
                group=description,
                icon="history" if description == "Prompt history" else "sparkles",
                sort=score,
            )
        )
    return items


def _configured_snippets(ctx: omp.Context) -> tuple[str, ...]:
    # GAP: omp.Context.settings is absent from the frozen layer (docs/py/00-overview.md §Manifest).
    raw = ctx.settings.get("snippets", "[]")
    decoded = json.loads(raw) if isinstance(raw, str) else raw
    if not isinstance(decoded, list) or any(not isinstance(item, str) for item in decoded):
        raise ValueError("the snippets setting must be a JSON array of strings")
    return tuple(item.strip() for item in decoded if item.strip())


@ui.completion(
    ui.Trigger(
        prefix="",
        at_line_start=True,
        min_chars=0,
        max_results=_MAX_RESULTS,
        refine_locally=True,
    )
)
async def prompt_suggestions(query: str, ctx: omp.Context) -> list[ui.CompletionItem]:
    """Supply recent prompts and configured snippets for native ranking and ghost hints."""

    history_rows: list[object] = []
    session_filter = sessions.SessionFilter(limit=_MAX_SESSIONS)
    for session in await sessions.list(session_filter):
        rows: list[object] = []
        since_index = max(0, session.entries - _MAX_ROWS_PER_SESSION)
        since = omp.EntryId(session.id, since_index) if since_index else None
        async for row in sessions.journal(
            session.id,
            kinds=("omp.message",),
            since=since,
            live=True,
        ):
            rows.append(row)
        history_rows.extend(reversed(rows))
    return _rank_candidates(query, history_rows, _configured_snippets(ctx))
