"""Render cached, model-rewritten assistant prose without changing the journal."""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

import omp
from omp import agents, context, ui
from omp.provider import Role

_CACHE_VERSION = 1
_DEFAULT_ROLE = "smol"
_DEFAULT_RULES = (
    "Improve clarity and scanability while preserving every fact, qualification, code "
    "block, quotation, link, and instruction. Do not add commentary."
)
_MAX_OUTPUT_TOKENS = 4_096
_SCOPE = omp.StateScope.SESSION


class _AssistantMessage(Protocol):
    """Describe the fields this renderer needs from the host message projection."""

    text: str


@omp.entry_kind("examples.legible.toggle", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class LegibleToggle:
    """Record whether rewritten assistant presentation is enabled in this session."""

    enabled: bool


_enabled = True
_rewrites: dict[str, str | None] = {}
_cache_root: Path | None = None


def _message_digest(text: str) -> str:
    """Hash the original UTF-8 message bytes used as the cache identity."""

    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _message_text(message: object) -> str | None:
    """Read assistant text without retaining or modifying its host-owned projection."""

    text = getattr(message, "text", None)
    return text if isinstance(text, str) else None


def _cache_path(root: Path, digest: str) -> Path:
    """Return the versioned cache path for one message digest."""

    return root / "legible-v1" / f"{digest}.json"


def _read_cache(root: Path) -> dict[str, str | None]:
    """Load valid rewrite records before any renderer fold reads the memory cache."""

    records: dict[str, str | None] = {}
    directory = root / "legible-v1"
    if not directory.is_dir():
        return records
    for path in directory.glob("*.json"):
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
            digest = value["digest"]
            rewrite = value["rewrite"]
            if (
                value.get("version") == _CACHE_VERSION
                and isinstance(digest, str)
                and path.stem == digest
                and (rewrite is None or isinstance(rewrite, str))
            ):
                records[digest] = rewrite
        except (OSError, UnicodeError, json.JSONDecodeError, KeyError, TypeError):
            continue
    return records


def _write_cache(root: Path, digest: str, rewrite: str | None) -> None:
    """Atomically persist one completed rewrite attempt under its message digest."""

    path = _cache_path(root, digest)
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = json.dumps(
        {"version": _CACHE_VERSION, "digest": digest, "rewrite": rewrite},
        ensure_ascii=False,
        separators=(",", ":"),
    )
    temporary = path.with_suffix(".tmp")
    temporary.write_text(payload, encoding="utf-8")
    temporary.replace(path)


def _parts_text(parts: list[omp.Part]) -> str:
    """Join only durable text parts, preserving their bytes and order."""

    return "".join(part.text for part in parts if isinstance(part, omp.TextPart))


def _toggle_value(record: object) -> bool | None:
    """Unwrap one typed state record when it contains a legible toggle."""

    value = getattr(record, "value", None)
    return value.enabled if isinstance(value, LegibleToggle) else None


@omp.message_renderer("assistant")
def render_assistant(message: _AssistantMessage, ctx: ui.RenderCtx) -> ui.Tml | None:
    """Render a cached rewrite, or select native rendering while it is unavailable."""

    del ctx
    if not _enabled:
        return None
    original = _message_text(message)
    if original is None:
        return None
    digest = _message_digest(original)
    rewrite = _rewrites.get(digest)
    if rewrite is None:
        return None
    return ui.md(rewrite)


async def _rewrite_once(original: str, ctx: omp.Context) -> None:
    """Compute and cache at most one model rewrite for these original bytes."""

    if not original:
        return
    digest = _message_digest(original)
    if digest in _rewrites:
        return

    role = str(ctx.settings.get("role", _DEFAULT_ROLE)).strip() or _DEFAULT_ROLE
    rules = str(ctx.settings.get("rules", _DEFAULT_RULES)).strip() or _DEFAULT_RULES
    rewrite: str | None = None
    try:
        async with context.lane():
            result = await agents.completion(
                {"assistant_text": original},
                role=role,
                system=(
                    "Rewrite the supplied assistant text for legibility. Preserve its "
                    "meaning exactly and return only the rewritten text.\n\nRules:\n" + rules
                ),
                default="",
                scope="session",
                max_output_tokens=_MAX_OUTPUT_TOKENS,
                deadline=omp.Duration("10s"),
                labels={"feature": "legible"},
            )
        candidate = result.text.strip()
        if not result.fell_back and candidate:
            rewrite = candidate
    except Exception:
        rewrite = None

    # Store failed attempts too: a digest consumes no more than one completion.
    _rewrites[digest] = rewrite
    if _cache_root is not None:
        _write_cache(_cache_root, digest, rewrite)


@omp.hook("extension_activate", phase=omp.HookPhase.OBSERVE)
async def activate_legible(event: omp.ExtensionActivateEvent, ctx: omp.Context) -> None:
    """Load the rebuildable cache and latest session toggle before serving folds."""

    del event, ctx
    global _cache_root, _enabled
    state_path = await omp.state_dir()
    _cache_root = state_path.local_path()
    _rewrites.clear()
    _rewrites.update(_read_cache(_cache_root))
    record = await omp.state.latest(LegibleToggle, scope=_SCOPE)
    stored = _toggle_value(record)
    _enabled = True if stored is None else stored


@omp.hook("turn_end", phase=omp.HookPhase.OBSERVE, timeout=omp.Duration("15s"))
async def rewrite_assistant_turn(event: omp.TurnEndEvent, ctx: omp.Context) -> None:
    """Rewrite newly committed assistant messages outside the synchronous fold."""

    if not _enabled:
        return
    assistant_ids = {
        item.item_id
        for item in event.items
        if item.kind is omp.ItemKind.MESSAGE and item.role is Role.ASSISTANT
    }
    if not assistant_ids:
        return
    view = await context.view()
    for message in view.messages:
        if message.id not in assistant_ids:
            continue
        original = _parts_text(await message.parts())
        await _rewrite_once(original, ctx)


@omp.command("legible", description="Toggle rewritten assistant presentation")
async def toggle_legible(invocation: ui.Invocation, ctx: omp.Context) -> ui.Consumed:
    """Flip and durably record this session's presentation toggle."""

    del invocation, ctx
    global _enabled
    enabled = not _enabled
    await omp.state.append(LegibleToggle(enabled), scope=_SCOPE)
    _enabled = enabled
    state = "enabled" if enabled else "disabled"
    return ui.Consumed(notice=ui.text(f"Legible assistant rendering {state}."))
