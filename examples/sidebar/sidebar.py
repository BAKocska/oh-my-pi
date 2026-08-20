from __future__ import annotations

from collections.abc import Hashable, Mapping
from dataclasses import dataclass, replace
from hashlib import blake2s

import omp
from omp import ui

_RAIL_KEY = "session-rail"
_ROW_LIMIT = 8
_RAIL = ui.SlotOptions(
    order=200,
    width=34,
    min_width=120,
    min_height=20,
    focusable=False,
    collapse=ui.Collapse.SHRINK,
)


@dataclass(frozen=True, slots=True)
class _AgentFacts:
    session: str
    parent: str | None
    agent: str
    depth: int
    model: str = ""
    turns: int = 0
    status: str = "idle"
    tokens: int = 0
    context_percent: int = 0


@dataclass(frozen=True, slots=True)
class _Patch:
    id: str
    text: str
    props: tuple[tuple[str, object], ...] = ()


_nodes: dict[str, _AgentFacts] = {}
_mounted = False
_last_hash: bytes | None = None
_last_patches: tuple[_Patch, ...] = ()


def _value(value: object, name: str, default: object = None) -> object:
    if isinstance(value, Mapping):
        return value.get(name, default)
    return getattr(value, name, default)


def _kind(payload: object) -> str:
    return str(_value(payload, "kind", ""))


def _coalesce_key(payload: object) -> Hashable:
    return (_kind(payload), str(_value(payload, "session", "unknown")))


def _model_name(value: object) -> str:
    if value is None:
        return ""
    model = _value(value, "model", None)
    return str(model if model is not None else value)


def _token_total(payload: object) -> int | None:
    tokens = _value(payload, "tokens", None)
    if tokens is None:
        return None
    total = _value(tokens, "total", None)
    if total is not None:
        return int(total)
    return int(_value(tokens, "input", 0)) + int(_value(tokens, "output", 0))


def _context_percent(payload: object) -> int | None:
    context = _value(payload, "context", None)
    if context is None:
        return None
    percent = _value(context, "percent", None)
    if percent is not None:
        return max(0, min(100, round(100 * float(percent))))
    prompt = int(_value(context, "prompt_tokens", 0))
    window = int(_value(context, "window", 0))
    return max(0, min(100, 100 * prompt // max(window, 1)))


def _apply(payload: object) -> None:
    session = str(_value(payload, "session", ""))
    if not session:
        return

    current = _nodes.get(
        session,
        _AgentFacts(
            session=session,
            parent=None,
            agent=str(_value(payload, "agent", "main")),
            depth=int(_value(payload, "depth", 0)),
        ),
    )
    kind = _kind(payload)
    updates: dict[str, object] = {
        "agent": str(_value(payload, "agent", current.agent)),
        "depth": int(_value(payload, "depth", current.depth)),
    }

    if kind == "session_start":
        parent = _value(payload, "parent", current.parent)
        updates.update(
            parent=None if parent is None else str(parent),
            model=_model_name(_value(payload, "model", current.model)),
            status="idle",
        )
    elif kind == "turn_start":
        updates.update(
            turns=max(current.turns, int(_value(payload, "turn", current.turns)) + 1),
            model=_model_name(_value(payload, "model", current.model)),
            status="working",
        )
    elif kind == "turn_end":
        updates.update(
            turns=max(current.turns, int(_value(payload, "turn", current.turns)) + 1),
            status="idle",
        )
    elif kind == "session_end":
        updates.update(
            turns=max(current.turns, int(_value(payload, "turns", current.turns))),
            status=str(_value(payload, "reason", "ended")),
        )

    tokens = _token_total(payload)
    if tokens is not None:
        updates["tokens"] = tokens
    context_percent = _context_percent(payload)
    if context_percent is not None:
        updates["context_percent"] = context_percent
    _nodes[session] = replace(current, **updates)


def _ordered_nodes() -> tuple[_AgentFacts, ...]:
    children: dict[str | None, list[_AgentFacts]] = {}
    for node in _nodes.values():
        children.setdefault(node.parent, []).append(node)
    for siblings in children.values():
        siblings.sort(key=lambda node: (node.agent.casefold(), node.session))

    ordered: list[_AgentFacts] = []
    seen: set[str] = set()

    def visit(node: _AgentFacts) -> None:
        if node.session in seen:
            return
        seen.add(node.session)
        ordered.append(node)
        for child in children.get(node.session, ()):
            visit(child)

    roots = sorted(
        (node for node in _nodes.values() if node.parent not in _nodes),
        key=lambda node: (node.depth, node.agent.casefold(), node.session),
    )
    for root in roots:
        visit(root)
    for node in sorted(_nodes.values(), key=lambda item: (item.depth, item.agent, item.session)):
        visit(node)
    return tuple(ordered)


def _patches() -> tuple[_Patch, ...]:
    ordered = _ordered_nodes()
    root = next((node for node in ordered if node.depth == 0), ordered[0] if ordered else None)
    if root is None:
        facts = (
            _Patch("session", "waiting for session facts"),
            _Patch("model", "—"),
            _Patch("turns", "0"),
            _Patch("context", "0%", (("fg", ui.Token.MUTED),)),
            _Patch("tokens", "0"),
        )
    else:
        tone = (
            ui.Token.ERR
            if root.context_percent > 90
            else ui.Token.WARN
            if root.context_percent > 70
            else ui.Token.MUTED
        )
        facts = (
            _Patch("session", root.session[-12:]),
            _Patch("model", root.model or "—"),
            _Patch("turns", str(root.turns)),
            _Patch("context", f"{root.context_percent}%", (("fg", tone),)),
            _Patch("tokens", f"{root.tokens:,}"),
        )

    rows: list[_Patch] = []
    for index in range(_ROW_LIMIT):
        if index >= len(ordered):
            text = ""
        else:
            node = ordered[index]
            marker = "working" if node.status == "working" else node.status
            text = f"{'  ' * min(node.depth, 3)}{node.agent} · {marker} · t{node.turns}"
        rows.append(_Patch(f"tree-{index}", text))
    return facts + tuple(rows)


def _digest(patches: tuple[_Patch, ...]) -> bytes:
    encoded = repr(patches).encode("utf-8")
    return blake2s(encoded, digest_size=16).digest()


def _markup(patches: tuple[_Patch, ...]) -> ui.Tml:
    values = {patch.id: patch.text for patch in patches}
    context_props = dict(next(patch.props for patch in patches if patch.id == "context"))
    rows = [
        ui.tml(
            f"<row><text id='tree-{index}' fg=secondary truncate>{{value}}</text></row>",
            value=values[f"tree-{index}"],
        )
        for index in range(_ROW_LIMIT)
    ]
    return ui.tml(
        "<col pad='0 1' gap=1 noselect>"
        "<row gap=1>{tree_icon}<text bold fg=accent>Sessions</text></row>"
        "<hr/>"
        "<table gap=1>"
        "<tr><td><text fg=muted>session</text></td><td><text id='session' truncate=start>{session}</text></td></tr>"
        "<tr><td><text fg=muted>model</text></td><td><text id='model' truncate=start>{model}</text></td></tr>"
        "<tr><td><text fg=muted>turns</text></td><td><text id='turns'>{turns}</text></td></tr>"
        "<tr><td><text fg=muted>ctx</text></td><td><text id='context' fg={tone}>{context}</text></td></tr>"
        "<tr><td><text fg=muted>tokens</text></td><td><text id='tokens'>{tokens}</text></td></tr>"
        "</table>"
        "<hr title='tree'/>"
        "{rows}"
        "<spacer grow/>"
        "<row gap=1>{layout_icon}<text dim>user layout wins</text></row>"
        "</col>",
        tree_icon=ui.icon("branch"),
        session=values["session"],
        model=values["model"],
        turns=values["turns"],
        tone=context_props["fg"],
        context=values["context"],
        tokens=values["tokens"],
        rows=rows,
        layout_icon=ui.icon("layout"),
    )


def _paint() -> int:
    global _last_hash, _last_patches, _mounted

    patches = _patches()
    state_hash = _digest(patches)
    if state_hash == _last_hash:
        return 0

    if not _mounted:
        ui.mount(ui.Slot.SIDEBAR_RIGHT, _markup(patches), _RAIL, key=_RAIL_KEY)
        effects = 1
        _mounted = True
    else:
        previous = {patch.id: patch for patch in _last_patches}
        handle = ui.handle(_RAIL_KEY)
        effects = 0
        for patch in patches:
            if previous.get(patch.id) == patch:
                continue
            handle.patch(patch.id, text=ui.text(patch.text), **dict(patch.props))
            effects += 1

    _last_patches = patches
    _last_hash = state_hash
    return effects


@omp.hook("extension_activate")
async def seed_sidebar(payload: object, ctx: omp.Context) -> None:
    """Mount a useful root row immediately, before replayed telemetry arrives."""
    del payload
    model = ctx.model.model if ctx.model is not None else ""
    _nodes.setdefault(
        ctx.session,
        _AgentFacts(
            session=ctx.session,
            parent=None,
            agent="main",
            depth=0,
            model=model,
        ),
    )
    _paint()


@omp.telemetry(
    ["session_start", "turn_start", "turn_end", "session_end"],
    scope=omp.telemetry.Scope.TREE,
    queue=256,
    overflow=omp.telemetry.Overflow.COALESCE_BY_KEY,
    coalesce_key=_coalesce_key,
    replay=True,
    replay_limit=512,
)
async def update_sidebar(payload: object, ctx: omp.Context) -> None:
    """Fold one coalesced tree fact into retained, keyed rail patches."""
    del ctx
    _apply(payload)
    _paint()
