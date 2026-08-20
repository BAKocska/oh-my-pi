"""Typed, data-only extension UI API.

Importing this module only creates immutable values and registration tables.  Effects
are encoded synchronously into the host-installed CONTROL sink; they never await the
renderer.
"""
from __future__ import annotations

from collections.abc import AsyncIterator as _AsyncIterator
from collections.abc import Callable, Iterable, Sequence
from contextvars import ContextVar
import inspect as _inspect
from dataclasses import dataclass, field, fields, is_dataclass
from enum import StrEnum
from string import Formatter
from typing import Any

from .._errors import ExtensionError as _ExtensionError
from .._registry import registry as _declarations


class TmlError(ValueError):
    """Markup source was malformed before it could be sent to the renderer."""

    def __init__(self, message: str, at: int, source: str) -> None:
        super().__init__(message)
        self.message, self.at, self.source = message, at, source


class SlotDenied(PermissionError):
    """The extension is not allowed to mount in the requested slot."""


class CommandDenied(PermissionError):
    """The extension is not allowed to register the requested command."""


class ShortcutError(ValueError):
    """A shortcut is malformed or unavailable."""


_SHORTCUT_MODIFIERS = ("ctrl", "alt", "shift", "super")
_SHORTCUT_KEYS = frozenset(
    (
        "enter", "tab", "backspace", "delete", "insert", "escape", "space",
        "up", "down", "left", "right", "home", "end", "pageup", "pagedown",
        *(f"f{number}" for number in range(1, 25)),
    )
)


def _normalize_shortcut_chord(chord: str) -> str:
    if not isinstance(chord, str):
        raise ShortcutError("shortcut chord must be a string")
    if not chord or chord != chord.strip():
        raise ShortcutError(f"malformed shortcut chord: {chord!r}")
    normalized = chord.lower()
    if normalized == "+":
        modifiers, key = [], "+"
    elif normalized.endswith("++") and normalized[:-2]:
        modifiers, key = normalized[:-2].split("+"), "+"
    else:
        parts = normalized.split("+")
        modifiers, key = parts[:-1], parts[-1]
    if (
        not key
        or any(not modifier or modifier not in _SHORTCUT_MODIFIERS for modifier in modifiers)
        or len(set(modifiers)) != len(modifiers)
    ):
        raise ShortcutError(f"malformed shortcut chord: {chord!r}")
    if key not in _SHORTCUT_KEYS and not (
        len(key) == 1 and key.isprintable() and not key.isspace()
    ):
        raise ShortcutError(f"malformed shortcut chord: {chord!r}")
    ordered_modifiers = (
        modifier for modifier in _SHORTCUT_MODIFIERS if modifier in modifiers
    )
    return "+".join((*ordered_modifiers, key))


class DialogUnavailable(RuntimeError):
    """An arbitrary overlay has no attached presentation client."""


class DuplicateRenderer(_ExtensionError):
    """A second device-rendering fold claimed the same frozen identity."""

@dataclass(frozen=True, slots=True)
class StatusFacts:
    """Read-only session facts used to render retained status chrome."""

    model: str
    context_tokens: int
    context_window: int
    cost_usd: float
    total_tokens: int
    tokens_per_second: float
    dropped: int = 0


def _validate(source: str) -> None:
    """Reject structurally malformed TML without admitting a second markup grammar."""
    stack: list[tuple[str, int]] = []
    cursor = 0
    while cursor < len(source):
        opening = source.find("<", cursor)
        if opening < 0:
            break
        if opening and source[opening - 1] == "\\":
            cursor = opening + 1
            continue
        closing = source.find(">", opening + 1)
        if closing < 0:
            raise TmlError("unclosed tag", opening, source)
        body = source[opening + 1 : closing].strip()
        if not body:
            raise TmlError("empty tag", opening, source)
        if body.startswith("/"):
            name = body[1:].strip()
            if not stack or stack[-1][0] != name:
                raise TmlError(f"unexpected closing tag </{name}>", opening, source)
            stack.pop()
        elif not body.endswith("/") and not body.startswith("!"):
            name = body.split(None, 1)[0]
            stack.append((name, opening))
        cursor = closing + 1
    if stack:
        name, at = stack[-1]
        raise TmlError(f"unclosed tag <{name}>", at, source)


@dataclass(frozen=True, slots=True, repr=False)
class Tml:
    """Opaque, already-validated markup source."""

    _source: str

    def __post_init__(self) -> None:
        _validate(self._source)

    @property
    def source(self) -> str:
        """Return the wire-format markup source after validation."""
        return self._source

    @classmethod
    def raw(cls, source: str) -> "Tml":
        """Ingest extension-authored wire markup after synchronous validation."""
        if not isinstance(source, str):
            raise TypeError("Tml.raw expects str")
        return cls(source)


def _clean_text(value: object) -> str:
    return "".join(ch for ch in str(value) if ch == "\t" or ord(ch) >= 32)


def text(value: object) -> Tml:
    """Create one escaped literal text leaf."""
    return Tml.raw("<text>" + _clean_text(value).replace("\\", "\\\\").replace("<", "\\<") + "</text>")


def md(source: object) -> Tml:
    """Create one Markdown leaf after removing terminal control characters."""
    return Tml.raw("<md>" + _clean_text(source).replace("<", "\\<") + "</md>")


def _field(value: object) -> str:
    if isinstance(value, Tml):
        return value.source
    if isinstance(value, str):
        return _clean_text(value).replace("\\", "\\\\").replace("<", "\\<")
    if isinstance(value, Sequence) and not isinstance(value, (bytes, bytearray)):
        if all(isinstance(part, Tml) for part in value):
            return "".join(part.source for part in value)
        if all(isinstance(part, str) for part in value):
            return " ".join(_field(part) for part in value)
    return _field(str(value)) if not isinstance(value, str) else value


def tml(template: str, /, **fields: object) -> Tml:
    """Build validated TML, escaping ordinary placeholder values."""
    parts: list[str] = []
    formatter = Formatter()
    for literal, name, format_spec, conversion in formatter.parse(template):
        if format_spec or conversion:
            raise ValueError("TML placeholders do not support format specs or conversions")
        parts.append(literal)
        if name is not None:
            if name not in fields:
                raise KeyError(name)
            parts.append(_field(fields[name]))
    return Tml.raw("".join(parts))


def join(parts: Iterable[Tml], sep: Tml | str = "") -> Tml:
    """Compose validated nodes, treating a string separator as literal text."""
    separator = sep.source if isinstance(sep, Tml) else text(sep).source
    return Tml.raw(separator.join(part.source for part in parts))


def icon(name: str, *, fg: str | None = None) -> Tml:
    """Create an icon node without choosing a glyph codepoint."""
    return Tml.raw(f"<ico:{name}/>" if fg is None else f"<icon icon={name} fg={fg}/>")


class Token(StrEnum):
    """Semantic theme token."""
    FG = "fg"; ACCENT = "accent"; INFO = "info"; OK = "ok"; WARN = "warn"; ERR = "err"; MUTED = "muted"; BORDER = "border"; SURFACE = "surface"; HOVER = "hover"; SELECTION = "selection"; SHADOW = "shadow"; PANEL = "panel"; SECONDARY = "secondary"; CONTRAST = "contrast"


class Charset(StrEnum):
    """Active glyph tier."""
    UNICODE = "unicode"; NERD_FONT = "nerd"; ASCII = "ascii"


class Appearance(StrEnum):
    """Terminal background appearance."""
    DARK = "dark"; LIGHT = "light"


class Graphics(StrEnum):
    """Image protocol available to the active presentation."""
    CELLS = "cells"; SIXEL = "sixel"; KITTY_PLACEHOLDERS = "kitty_placeholders"; KITTY_DIRECT = "kitty_direct"; ITERM2 = "iterm2"


class Slot(StrEnum):
    """Named, layout-owned extension mount point."""
    STATUS_LEFT = "status_left"; STATUS_RIGHT = "status_right"; HEADER = "header"; FOOTER = "footer"; ABOVE_EDITOR = "above_editor"; BELOW_EDITOR = "below_editor"; SIDEBAR_LEFT = "sidebar_left"; SIDEBAR_RIGHT = "sidebar_right"


class Collapse(StrEnum):
    """Mount behavior when the viewport does not fit it."""
    HIDE = "hide"; TRUNCATE = "truncate"; SHRINK = "shrink"


class Phase(StrEnum):
    """User-visible agent presentation phase."""
    IDLE = "idle"; WORKING = "working"; WAITING = "waiting"; ERROR = "error"


class Level(StrEnum):
    """Notice severity, including the documented ``warning`` string alias."""
    DEBUG = "debug"; INFO = "info"; WARN = "warn"; WARNING = "warn"; ERROR = "error"

    @classmethod
    def _missing_(cls, value: object) -> Level | None:
        return cls.WARN if value == "warning" else None


class Urgency(StrEnum):
    """Desktop-notification urgency."""
    LOW = "low"; NORMAL = "normal"; CRITICAL = "critical"


class Sound(StrEnum):
    """Terminal or desktop notification sound class."""
    SILENT = "silent"; SYSTEM = "system"; INFO = "info"; WARNING = "warning"; ERROR = "error"; QUESTION = "question"


class Anchor(StrEnum):
    """Overlay anchor point."""
    CENTER = "center"; TOP_LEFT = "top_left"; TOP = "top"; TOP_RIGHT = "top_right"; RIGHT = "right"; BOTTOM_RIGHT = "bottom_right"; BOTTOM = "bottom"; BOTTOM_LEFT = "bottom_left"; LEFT = "left"


class ActivationSource(StrEnum):
    """Input source that activated a focusable transcript element."""

    KEY = "key"
    MOUSE = "mouse"

class EventKind(StrEnum):
    """Observed retained-overlay interaction."""
    HIGHLIGHTED = "highlighted"; CHANGED = "changed"; FILTERED = "filtered"; PRESSED = "pressed"; SUBMIT = "submit"; CANCEL = "cancel"


class DialogCancel(StrEnum):
    """Reason a dialog returned a cancelled outcome."""
    DISMISSED = "dismissed"; TIMED_OUT = "timed_out"; UNAVAILABLE = "unavailable"; SUPERSEDED = "superseded"


class InvocationMode(StrEnum):
    """Client mode in which a command was invoked."""
    INTERACTIVE = "interactive"; HEADLESS = "headless"; RPC = "rpc"


class RenderPlace(StrEnum):
    """Presentation surface requesting a fold."""
    TRANSCRIPT = "transcript"; OVERLAY = "overlay"; SLOT = "slot"; EXPORT = "export"




class Marker(StrEnum):
    """Leading marker style for native choice dialogs."""
    RADIO = "radio"; CHECKBOX = "checkbox"


@dataclass(frozen=True, slots=True)
class Margin:
    """Insets from the four viewport edges."""
    top: int = 0; right: int = 0; bottom: int = 0; left: int = 0


@dataclass(frozen=True, slots=True)
class Pct:
    """A percentage viewport dimension."""
    value: int


@dataclass(frozen=True, slots=True)
class Progress:
    """Terminal taskbar progress state."""
    kind: str
    pct: int | None = None
    @classmethod
    def clear(cls) -> "Progress": return cls("clear")
    @classmethod
    def value(cls, pct: int) -> "Progress": return cls("value", pct)
    @classmethod
    def error(cls, pct: int) -> "Progress": return cls("error", pct)
    @classmethod
    def indeterminate(cls) -> "Progress": return cls("indeterminate")
    @classmethod
    def paused(cls, pct: int) -> "Progress": return cls("paused", pct)
@dataclass(frozen=True, slots=True)
class Presentation:
    """Read-only current terminal presentation facts."""
    charset: Charset = Charset.UNICODE; appearance: Appearance = Appearance.DARK; width: int = 0; height: int = 0; graphics: Graphics = Graphics.CELLS; hyperlinks: bool = False; has_ui: bool = False


@dataclass(frozen=True, slots=True)
class RenderCtx:
    """Read-only presentation facts handed to a pure rendering fold."""
    width: int; charset: Charset; appearance: Appearance; graphics: Graphics; hyperlinks: bool; focused: bool; collapsed: bool; place: RenderPlace


@dataclass(frozen=True, slots=True)
class SlotOptions:
    """Responsive layout options for a slot mount."""
    order: int = 100; width: int | None = None; min_width: int = 0; min_height: int = 0; max_height: int | None = None; visible_in: frozenset[Phase] = frozenset(Phase); focusable: bool = False; collapse: Collapse = Collapse.HIDE; title: str | None = None


@dataclass(frozen=True, slots=True)
class OverlayOptions:
    """Viewport-relative overlay placement options."""
    width: int | Pct | None = None; min_width: int | None = None; max_height: int | Pct | None = None; anchor: Anchor = Anchor.CENTER; offset_x: int = 0; offset_y: int = 0; row: int | Pct | None = None; col: int | Pct | None = None; margin: Margin = Margin(); z: int = 0; min_viewport: tuple[int, int] = (0, 0); modal: bool = True; fill_height: bool = False


@dataclass(frozen=True, slots=True)
class DialogOptions:
    """Common native-dialog options."""
    timeout: Any | None = None; timeout_starts_on_present: bool = True; countdown: bool = True; initial: int = 0; marker: Marker | None = None; help: str | None = None; overlay: OverlayOptions | None = None; context: Tml | None = None


@dataclass(frozen=True, slots=True)
class SelectItem:
    """One typed choice in a dialog or picker."""
    value: str; label: str | None = None; desc: str | None = None; preview: Tml | None = None; cells: tuple[str, ...] = (); recommended: bool = False; group: str | None = None


@dataclass(frozen=True, slots=True)
class Field:
    """One typed native form field."""
    id: str; kind: str; label: str; desc: str | None = None; value: object | None = None; options: tuple[SelectItem, ...] = (); min: int | None = None; max: int | None = None; step: int | None = None; required: bool = False; match: str | None = None


@dataclass(frozen=True, slots=True)
class OverlayEvent:
    """One watched retained-overlay interaction."""
    kind: EventKind; id: str | None = None; value: str | None = None; query: str | None = None; values: dict[str, object] = field(default_factory=dict)


def _overlay_event(value: object) -> OverlayEvent:
    if isinstance(value, OverlayEvent):
        return value
    if not isinstance(value, dict):
        raise TypeError("overlay event frames must be OverlayEvent or dict values")
    body = dict(value)
    body["kind"] = EventKind(body["kind"])
    return OverlayEvent(**body)


@dataclass(frozen=True, slots=True)
class DialogOutcome:
    """Total result of a UI dialog request."""
    cancelled: bool; reason: DialogCancel | None = None; confirmed: bool = False; value: str | None = None; values: tuple[str, ...] = (); fields: dict[str, object] = field(default_factory=dict); answers: tuple["AskAnswer", ...] = (); elapsed: Any | None = None
    def __bool__(self) -> bool: return not self.cancelled and self.confirmed


@dataclass(frozen=True, slots=True)
class AskQuestion:
    """One question in an ask-user dialog."""
    id: str; question: str; header: str | None = None; context: Tml | None = None; options: tuple[SelectItem, ...] = (); multi: bool = False; allow_freeform: bool = True; allow_note: bool = False; recommended: str | None = None


@dataclass(frozen=True, slots=True)
class AskAnswer:
    """One durable answer from an ask-user dialog."""
    question_id: str; selected: tuple[str, ...] = (); freeform: str | None = None; note: str | None = None; timed_out: bool = False


@dataclass(frozen=True, slots=True)
class Ghost:
    """Push-only inline composer suggestion."""
    text: str; id: str | None = None; only_when_empty: bool = True; expires: Any | None = None


@dataclass(frozen=True, slots=True)
class Trigger:
    """Static completion trigger declaration."""
    prefix: str; at_line_start: bool = False; min_chars: int = 0; debounce: Any = "90ms"; max_results: int = 20; cache: Any = "2s"; refine_locally: bool = True


@dataclass(frozen=True, slots=True)
class CompletionItem:
    """One completion row returned by a completion fold."""
    insert: str; label: str | None = None; desc: str | None = None; hint: str | None = None; group: str | None = None; icon: str | None = None; sort: int = 0


@dataclass(frozen=True, slots=True)
class Action:
    """One shortcut activation."""
    action_id: str; chord: str; phase: Phase


@dataclass(frozen=True, slots=True)
class Activation:
    """One click or Enter activation from an id-bearing transcript element."""

    element_id: str
    source: ActivationSource


@dataclass(frozen=True, slots=True)
class Invocation:
    """A parsed slash-command invocation."""
    name: str; argv: tuple[str, ...]; raw: str; mode: InvocationMode


@dataclass(frozen=True, slots=True)
class Arg:
    """One static command argument declaration."""
    name: str; description: str = ""; usage: str | None = None


@dataclass(frozen=True, slots=True)
class ArgQuery:
    """Dynamic command-argument completion request."""
    prefix: str; argv: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class Consumed:
    """A slash command consumed locally, optionally with a durable notice."""
    notice: Tml | None = None


@dataclass(frozen=True, slots=True)
class Prompt:
    """A slash command result that supplies text to the composer or model."""
    text: str; submit: bool = True


class _Limits:
    """Read-only UI protocol ceilings."""
    TML_MAX_BYTES = 262_144; TML_MAX_DEPTH = 64; SLOT_MAX_PER_EXTENSION = 16; NOTIFY_PER_TURN = 10; COMPLETION_DEADLINE = "250ms"; RENDER_DEADLINE = "50ms"; OVERLAY_MAX_CONCURRENT = 2; WATCH_DEBOUNCE = "60ms"


limits = _Limits()
_effect_sink: ContextVar[Callable[[dict[str, object]], None] | None] = ContextVar("omp_ui_effect_sink", default=None)
_handles: dict[str, "SlotHandle"] = {}
_message_renderers: dict[str, Callable[..., Tml | None]] = {}
_completion_handlers: dict[str, Callable[..., object]] = {}
_shortcut_handlers: dict[str, Callable[..., object]] = {}
_device_renderers: dict[tuple[str, str, int], Callable[..., Tml | None]] = {}
_fold_failures: set[tuple[str, str]] = set()
_command_handlers: dict[str, Callable[..., object]] = {}
_activation_handlers: dict[str, Callable[..., object]] = {}


def _install_effect_sink(sink: Callable[[dict[str, object]], None] | None) -> None:
    """Install the host-owned synchronous effect queue for this context."""
    _effect_sink.set(sink)


def _wire(value: object) -> object:
    if isinstance(value, Tml):
        return {"source": value.source}
    if isinstance(value, StrEnum):
        return value.value
    if is_dataclass(value):
        return {item.name: _wire(getattr(value, item.name)) for item in fields(value)}
    if isinstance(value, dict):
        return {str(key): _wire(item) for key, item in value.items()}
    if isinstance(value, (tuple, list, frozenset)):
        return [_wire(item) for item in value]
    return value


def _emit(kind: str, **body: object) -> None:
    sink = _effect_sink.get()
    if sink is None:
        return
    try:
        sink({"kind": kind, "body": _wire(body)})
    except Exception:
        # Effects are explicitly fail-open: a dead renderer never breaks a turn.
        return


class OverlayHandle:
    """Async owner handle for one retained overlay."""
    __slots__ = ("id", "_hidden")
    def __init__(self, overlay_id: str) -> None: self.id, self._hidden = overlay_id, False
    def set(self, content: Tml) -> None: _emit("overlay_set", id=self.id, content=content)
    def patch(self, id: str, *, text: Tml | str | None = None, **props: object) -> None: _emit("overlay_patch", overlay=self.id, id=id, text=text, props=props)
    @property
    def hidden(self) -> bool: return self._hidden
    @hidden.setter
    def hidden(self, value: bool) -> None: self._hidden = bool(value); _emit("overlay_hidden", id=self.id, hidden=self._hidden)
    def focus(self) -> None: _emit("overlay_focus", id=self.id)
    def blur(self) -> None: _emit("overlay_blur", id=self.id)
    async def values(self) -> dict[str, object]:
        result = await _request("overlay_values", id=self.id)
        return result if isinstance(result, dict) else {}
    async def close(self) -> None: await _request("overlay_close", id=self.id)
    async def wait(self) -> DialogOutcome: return await _dialog("overlay_wait", id=self.id)
    async def events(self) -> _AsyncIterator[OverlayEvent]:
        """Yield only the watched and terminal interactions fed by the host."""

        source = await _request("overlay_events", id=self.id)
        if hasattr(source, "__aiter__"):
            async for event in source:
                yield _overlay_event(event)
        elif isinstance(source, Iterable) and not isinstance(
            source, (str, bytes, bytearray, dict)
        ):
            for event in source:
                yield _overlay_event(event)
    async def __aenter__(self) -> "OverlayHandle": return self
    async def __aexit__(self, *_: object) -> None: await self.close()


class SlotHandle:
    """Local handle for one extension-owned slot mount."""
    __slots__ = ("key", "placement", "_visible")
    def __init__(self, key: str, placement: Slot) -> None: self.key, self.placement, self._visible = key, placement, True
    def set(self, content: Tml) -> None: _emit("mount", key=self.key, placement=self.placement, content=content)
    def patch(self, id: str, *, text: Tml | str | None = None, **props: object) -> None: _emit("patch", key=self.key, id=id, text=text, props=props)
    @property
    def visible(self) -> bool: return self._visible
    @visible.setter
    def visible(self, value: bool) -> None: self._visible = bool(value); _emit("slot_visible", key=self.key, visible=self._visible)
    def unmount(self) -> None: _handles.pop(self.key, None); _emit("unmount", key=self.key)


def mount(placement: Slot, content: Tml, options: SlotOptions | None = None, *, key: str | None = None) -> SlotHandle:
    """Synchronously queue a coalescible slot mount or replacement."""
    if not isinstance(content, Tml): raise TypeError("slot content must be Tml")
    key = key or "default"
    handle = _handles.get(key) or SlotHandle(key, placement)
    _handles[key] = handle
    _emit("mount", key=key, placement=placement, content=content, options=options or SlotOptions())
    return handle


def handle(key: str) -> SlotHandle:
    """Return one local extension-owned slot handle."""
    return _handles[key]


def unmount(key: str) -> None:
    """Synchronously remove one extension-owned mount."""
    handle(key).unmount()


def unmount_all() -> None:
    """Synchronously remove all extension-owned mounts."""
    for mounted in tuple(_handles.values()): mounted.unmount()


def focus_slot(key: str) -> None: """Synchronously focus an eligible rail."""; _emit("focus_slot", key=key)
def blur_slot() -> None: """Synchronously return focus to the composer."""; _emit("blur_slot")
def set_status(key: str, content: Tml | None, *, order: int = 100, side: Slot = Slot.STATUS_RIGHT) -> None: """Synchronously update a status contribution."""; _emit("set_status", key=key, content=content, order=order, side=side)
def notify(message: str | Tml, *, level: Level | str = Level.INFO, title: str | None = None, desktop: bool = False, sound: Sound | None = None, urgency: Urgency | None = None) -> None: """Synchronously queue a fail-open notice effect."""; _emit("notify", message=message, level=Level(level), title=title, desktop=desktop, sound=sound, urgency=urgency)
def set_working_message(content: Tml | None) -> None: """Synchronously replace the working-message banner."""; _emit("set_working_message", content=content)
def set_title(title: str | None) -> None: """Synchronously update the terminal title."""; _emit("set_title", title=title)
def bell() -> None: """Synchronously queue one attention bell."""; _emit("bell")
def set_progress(state: Progress) -> None: """Synchronously set terminal taskbar progress."""; _emit("set_progress", state=state)
def image(source: object, *, w: int | None = None, h: int | None = None, trim: bool = False) -> Tml:
    """Queue image materialization and return its stable markup placeholder."""
    _emit("image", source=source, w=w, h=h, trim=trim)
    dimensions = "".join(value for value in (f" w={w}" if w is not None else "", f" h={h}" if h is not None else "") if value)
    return Tml.raw(f"<img{dimensions}/>")
def set_ghost(ghost: Ghost | None) -> None: """Synchronously replace the inline ghost suggestion."""; _emit("set_ghost", ghost=ghost)
def clear_ghost() -> None: """Synchronously clear the inline ghost suggestion."""; set_ghost(None)
def set_editor_text(text: str) -> None: """Synchronously replace composer text."""; _emit("set_editor_text", text=text)
def set_clipboard(text: str) -> None: """Synchronously queue a client-owned clipboard write."""; _emit("set_clipboard", text=text)
def paste_to_editor(content: object) -> None: """Synchronously route content through the composer paste pipeline."""; _emit("paste_to_editor", content=content)
def submit(text: str | None = None) -> None: """Synchronously submit the composer, optionally after replacement."""; _emit("submit", text=text)
def open_url(url: str) -> None: """Synchronously request a validated client-side URL open."""; _emit("open_url", url=url)


async def _request(kind: str, **body: object) -> object:
    from .. import _control_request
    try:
        return await _control_request("omp.ui." + kind, **_wire(body))
    except Exception:
        return None


def _unavailable() -> DialogOutcome:
    return DialogOutcome(cancelled=True, reason=DialogCancel.UNAVAILABLE)


async def presentation() -> Presentation:
    """Return current presentation facts, or a no-UI value when unavailable."""
    result = await _request("presentation")
    return Presentation(**result) if isinstance(result, dict) else Presentation()


async def icons(prefix: str = "") -> tuple[str, ...]:
    """Return catalog icon names matching an optional prefix."""
    result = await _request("icons", prefix=prefix)
    return tuple(result) if isinstance(result, (list, tuple)) else ()


async def editor_text() -> str:
    """Return composer text, or an empty value when unavailable."""
    result = await _request("editor_text")
    return result if isinstance(result, str) else ""


async def _dialog(kind: str, **body: object) -> DialogOutcome:
    result = await _request(kind, **body)
    if not isinstance(result, dict):
        return _unavailable()
    try:
        if "reason" in result and result["reason"] is not None:
            result["reason"] = DialogCancel(result["reason"])
        return DialogOutcome(**result)
    except (TypeError, ValueError):
        return _unavailable()


async def overlay(content: Tml, options: OverlayOptions | None = None, *, watch: Sequence[str] = ()) -> OverlayHandle:
    """Show a retained overlay or raise when no presentation client exists."""
    if not isinstance(content, Tml):
        raise TypeError("overlay content must be Tml")
    result = await _request("overlay", content=content, options=options or OverlayOptions(), watch=tuple(watch))
    if not isinstance(result, dict) or not isinstance(result.get("id"), str):
        raise DialogUnavailable("no UI or RPC dialog client is available")
    return OverlayHandle(result["id"])


async def confirm(title: str, message: str | Tml = "", *, options: DialogOptions | None = None) -> DialogOutcome: """Ask a total confirmation request."""; return await _dialog("confirm", title=title, message=message, options=options)
async def select(title: str, items: Sequence[SelectItem | str], *, options: DialogOptions | None = None) -> DialogOutcome: """Ask a total single-select request."""; return await _dialog("select", title=title, items=items, options=options)
async def multi_select(title: str, items: Sequence[SelectItem | str], *, checked: Sequence[str] = (), options: DialogOptions | None = None) -> DialogOutcome: """Ask a total multi-select request."""; return await _dialog("multi_select", title=title, items=items, checked=tuple(checked), options=options)
async def input(title: str, *, placeholder: str = "", prefill: str = "", mask: bool = False, match: str | None = None, options: DialogOptions | None = None) -> DialogOutcome: """Ask a total text-input request."""; return await _dialog("input", title=title, placeholder=placeholder, prefill=prefill, mask=mask, match=match, options=options)
async def editor(title: str, *, prefill: str = "", syntax: str | None = None, options: DialogOptions | None = None) -> DialogOutcome: """Ask a total editor request."""; return await _dialog("editor", title=title, prefill=prefill, syntax=syntax, options=options)
async def form(title: str, fields: Sequence[object], *, options: DialogOptions | None = None) -> DialogOutcome: """Ask a total form request."""; return await _dialog("form", title=title, fields=tuple(fields), options=options)
async def ask_user(questions: AskQuestion | Sequence[AskQuestion], *, options: DialogOptions | None = None) -> DialogOutcome: """Ask a total multi-question request."""; return await _dialog("ask_user", questions=questions, options=options)


def message_renderer(kind: str) -> Callable[[Callable[..., Tml | None]], Callable[..., Tml | None]]:
    """Register one synchronous, pure transcript-message fold."""
    def decorate(function: Callable[..., Tml | None]) -> Callable[..., Tml | None]:
        if not callable(function) or _inspect.iscoroutinefunction(function):
            raise TypeError("message_renderer folds must be synchronous callables")
        _message_renderers[kind] = function
        return function
    return decorate



def renderer(name: str, *, family: str | None = None, rev: int | None = None, reduce: Callable[[object, object], object] | None = None) -> Callable[[Callable[..., Tml | None]], Callable[..., Tml | None]]:
    """Register one exact-revision device-rendering fold."""
    if not name:
        raise ValueError("renderer name must not be empty")
    key = (name, family or "", 0 if rev is None else rev)
    def decorate(function: Callable[..., Tml | None]) -> Callable[..., Tml | None]:
        if _inspect.iscoroutinefunction(function):
            raise TypeError("renderer folds must be synchronous")
        if key in _device_renderers:
            raise DuplicateRenderer(f"duplicate renderer: {key!r}")
        function.__omp_renderer_reduce__ = reduce
        _device_renderers[key] = function
        return function
    return decorate


def _dispatch_renderer(name: str, family: str, rev: int, view: object, ctx: RenderCtx) -> Tml | None:
    """Run one exact device fold; failures select the native fallback."""
    function = _device_renderers.get((name, family, rev))
    if function is None:
        return None
    try:
        value = function(view, ctx)
        if value is not None and not isinstance(value, Tml):
            raise TypeError("renderer folds must return Tml or None")
        return value
    except Exception:
        _fold_failures.add((name, "renderer"))
        return None


def _dispatch_message_renderer(kind: str, message: object, ctx: RenderCtx) -> Tml | None:
    """Run one deadline-owned message fold; ``None`` selects native rendering."""
    function = _message_renderers.get(kind)
    if function is None:
        return None
    try:
        value = function(message, ctx)
        if value is not None and not isinstance(value, Tml):
            raise TypeError("message renderer must return Tml or None")
        return value
    except Exception:
        _fold_failures.add((kind, "message"))
        return None

def completion(trigger: Trigger) -> Callable[[Callable[..., object]], Callable[..., object]]:
    """Register one asynchronous completion fold by static trigger prefix."""
    def decorate(function: Callable[..., object]) -> Callable[..., object]: _completion_handlers[trigger.prefix] = function; return function
    return decorate


def on_activate(prefix: str) -> Callable[[Callable[..., object]], Callable[..., object]]:
    """Register a handler for transcript ids equal to or nested below ``prefix``."""

    if not isinstance(prefix, str) or not prefix:
        raise ValueError("activation prefix must be a non-empty string")

    def decorate(function: Callable[..., object]) -> Callable[..., object]:
        if not callable(function):
            raise TypeError("activation handlers must be callable")
        _activation_handlers[prefix] = function
        return function

    return decorate


async def _dispatch_activation(
    activation: Activation, ctx: object
) -> None:
    """Dispatch one host-originated transcript activation to its closest prefix."""

    if not isinstance(activation, Activation):
        raise TypeError("activation dispatch requires Activation")
    matches = (
        prefix
        for prefix in _activation_handlers
        if activation.element_id == prefix
        or activation.element_id.startswith(f"{prefix}.")
    )
    prefix = max(matches, key=len, default=None)
    if prefix is None:
        return
    result = _activation_handlers[prefix](activation, ctx)
    if _inspect.isawaitable(result):
        await result


def shortcut(chord: str, *, action_id: str | None = None, description: str = "", when: frozenset[Phase] | None = None) -> Callable[[Callable[..., object]], Callable[..., object]]:
    """Declare one validated shortcut with static dispatch metadata."""

    normalized_chord = _normalize_shortcut_chord(chord)

    def decorate(function: Callable[..., object]) -> Callable[..., object]:
        resolved_action_id = action_id or function.__name__
        _declarations.register_shortcut(
            normalized_chord,
            resolved_action_id,
            description,
            when,
            function,
        )
        _shortcut_handlers[resolved_action_id] = function
        return function

    return decorate


def command(name: str, *, aliases: Sequence[str] = (), description: str = "", args: Sequence[Arg] = (), hint: str | None = None, arg_completions: Callable[..., object] | None = None) -> Callable[[Callable[..., object]], Callable[..., object]]:
    """Declare one slash command with static and dynamic completion metadata."""
    resolved_aliases = tuple(aliases)
    resolved_args = tuple(args)
    if any(not isinstance(argument, Arg) for argument in resolved_args):
        raise TypeError("command args must contain only Arg values")
    def decorate(function: Callable[..., object]) -> Callable[..., object]:
        _declarations.register_command(
            name,
            resolved_aliases,
            description,
            resolved_args,
            hint,
            arg_completions,
            function,
        )
        _command_handlers[name] = function
        return function
    return decorate


__all__ = tuple(name for name in globals() if not name.startswith("_"))
