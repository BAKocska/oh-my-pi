"""Durable regime declarations and isolated middleware callback drafts.

Importing this module records declarations only. Runtime callbacks stage effects
through :class:`RegimeContext` and select at most one control through
:class:`Next`; no callback invokes another regime.
"""

from __future__ import annotations

import inspect
import json
from collections.abc import Callable, Mapping, Sequence
from dataclasses import asdict, dataclass, is_dataclass
from enum import StrEnum
from types import MappingProxyType
from typing import Any, Final, TypeVar

from _omp import Duration, OmpError

from ._errors import DeclarationSealed
from ._registry import registry
from .hooks import LateRegistration, OnFailure

_RegimeTarget = TypeVar("_RegimeTarget", bound=Callable[..., object] | type)
_REGIME_INSTANCES: dict[tuple[str, str], object] = {}


class RegimeContractError(OmpError, ValueError):
    """Report a declaration or callback that violates the regime contract."""


class StateSchemaMismatch(RegimeContractError):
    """Report durable regime state whose schema revision is incompatible."""

    def __init__(self, expected: int, actual: int | None) -> None:
        self.expected = expected
        self.actual = actual
        super().__init__(
            f"regime state revision mismatch: expected {expected}, got {actual!r}"
        )


class StateDecodeError(RegimeContractError):
    """Report durable regime state that cannot rebuild its declared dataclass."""


class Point(StrEnum):
    """Name one fixed event in the agent loop."""

    CONTEXT = "context"
    TOOL_CHOICE = "tool_choice"
    PRE_MODEL = "pre_model"
    STREAM = "stream"
    ADMISSION = "admission"
    BATCH = "batch"
    TURN_END = "turn_end"
    SETTLE = "settle"
    IDLE = "idle"


CONTEXT: Final[Point] = Point.CONTEXT
"""Provider-context projection event."""
TOOL_CHOICE: Final[Point] = Point.TOOL_CHOICE
"""Tool-choice resolution event."""
PRE_MODEL: Final[Point] = Point.PRE_MODEL
"""Pre-sampling event."""
STREAM: Final[Point] = Point.STREAM
"""Active model-stream event."""
ADMISSION: Final[Point] = Point.ADMISSION
"""Tool-call admission event."""
BATCH: Final[Point] = Point.BATCH
"""Active tool-batch event."""
TURN_END: Final[Point] = Point.TURN_END
"""Turn-boundary event."""
SETTLE: Final[Point] = Point.SETTLE
"""Agent-settlement event."""
IDLE: Final[Point] = Point.IDLE
"""Idle mailbox-boundary event."""


class RegimeLifetime(StrEnum):
    """Bound the lifetime of one regime activation."""

    TURN = "turn"
    RUN = "run"
    SESSION = "session"


_POINT_TABLE: Final[tuple[str, ...]] = tuple(point.value for point in Point)
_CONTROL_TABLE: Final[tuple[str, ...]] = (
    "retry",
    "wait",
    "reject",
    "cancel",
    "complete",
    "fail",
)
_EFFECT_TABLE: Final[tuple[str, ...]] = (
    "append_context",
    "rewrite_context",
    "require_tool",
    "set_scoped",
    "replace_state",
)
_ALLOWED_CONTROLS: Final[Mapping[Point, frozenset[str]]] = MappingProxyType(
    {
        Point.CONTEXT: frozenset(),
        Point.TOOL_CHOICE: frozenset(),
        Point.PRE_MODEL: frozenset({"wait"}),
        Point.STREAM: frozenset({"cancel"}),
        Point.ADMISSION: frozenset({"wait", "reject"}),
        Point.BATCH: frozenset({"reject", "cancel"}),
        Point.TURN_END: frozenset(),
        Point.SETTLE: frozenset({"retry", "complete", "fail"}),
        Point.IDLE: frozenset(),
    }
)


class _WhenNamespace:
    """Build data-only activation conditions evaluated by Core."""

    __slots__ = ()

    def checkpoint_active(self) -> Mapping[str, object]:
        """Activate at CONTEXT while a durable checkpoint is active."""
        return MappingProxyType(
            {"point": Point.CONTEXT.value, "checkpoint_active": True}
        )


when: Final[_WhenNamespace] = _WhenNamespace()
"""Data-only regime activation-condition builders."""


def user_text(text: str) -> Mapping[str, object]:
    """Build one canonical user-message thread item for context insertion."""
    if not isinstance(text, str) or not text:
        raise ValueError("user text must be a non-empty string")
    return MappingProxyType(
        {
            "seq": 0,
            "created_at_ms": 0,
            "message": {
                "role": "ROLE_USER",
                "parts": [{"text": text}],
            },
            "props": {},
        }
    )


@dataclass(frozen=True, slots=True)
class _StateSchema:
    family: str
    revision: int
    state_type: type

    def encode(self, value: object) -> bytes:
        if not isinstance(value, self.state_type):
            raise TypeError(f"state must be {self.state_type.__qualname__}")
        return _wire_bytes(asdict(value))

    def decode(
        self,
        payload: bytes | bytearray | memoryview | str,
        revision: int | None,
    ) -> object:
        if revision != self.revision:
            raise StateSchemaMismatch(self.revision, revision)
        try:
            raw = bytes(payload).decode("utf-8") if not isinstance(payload, str) else payload
            values = json.loads(raw)
        except (TypeError, UnicodeDecodeError, ValueError) as error:
            raise StateDecodeError("regime state payload is malformed") from error
        if not isinstance(values, Mapping):
            raise StateDecodeError("regime state fields must be an object")
        try:
            return self.state_type(**values)
        except (TypeError, ValueError) as error:
            raise StateDecodeError(
                "regime state fields do not match the declared dataclass"
            ) from error


@dataclass(frozen=True, slots=True)
class RegimeDeclaration:
    """Describe one import-time regime declaration sealed at FREEZE."""

    id: str
    points: tuple[Point, ...]
    target: Callable[..., object] | type
    lifetime: RegimeLifetime
    state: _StateSchema | None
    when: object | None
    max_steps: int | None
    on_limit: Callable[..., object] | None
    owns: tuple[str, ...]
    sets: Mapping[str, object]
    minimum_duration: Duration | None
    on_failure: OnFailure
    revision: int = 1
    precedence: int = 0


class RegimeEvent(Mapping[str, object]):
    """Expose the current fixed point and its immutable event payload."""

    __slots__ = ("point", "_payload")

    def __init__(self, point: Point, payload: Mapping[str, object]) -> None:
        self.point = point
        self._payload = MappingProxyType(dict(payload))

    def __getitem__(self, key: str) -> object:
        return self._payload[key]

    def __iter__(self):
        return iter(self._payload)

    def __len__(self) -> int:
        return len(self._payload)

    def __getattr__(self, name: str) -> object:
        try:
            return self._payload[name]
        except KeyError as error:
            raise AttributeError(name) from error


class _Draft:
    __slots__ = ("control", "effects")

    def __init__(self) -> None:
        self.control: dict[str, object] | None = None
        self.effects: list[dict[str, object]] = []


class _ContextEffects:
    __slots__ = ("_draft",)

    def __init__(self, draft: _Draft) -> None:
        self._draft = draft

    def append(self, *items: object) -> None:
        """Stage one or more canonical context items in call order."""
        if not items:
            raise ValueError("context.append requires at least one item")
        encoded = tuple(_context_item(item) for item in items)
        self._draft.effects.append(
            {"kind": "append_context", "payload": _wire_bytes(encoded), "props": {}}
        )

    def rewrite(self, patch: object) -> None:
        """Stage one ordered provider-context rewrite."""
        self._draft.effects.append(
            {"kind": "rewrite_context", "payload": _wire_bytes(patch), "props": {}}
        )


class _ToolEffects:
    __slots__ = ("_draft",)

    def __init__(self, draft: _Draft) -> None:
        self._draft = draft

    def require(self, name: str) -> None:
        """Stage exclusive selection of one advertised tool."""
        if not isinstance(name, str) or not name:
            raise ValueError("tool name must be a non-empty string")
        self._draft.effects.append(
            {"kind": "require_tool", "payload": b"", "name": name, "props": {}}
        )


class _SettingEffects:
    __slots__ = ("_draft",)

    def __init__(self, draft: _Draft) -> None:
        self._draft = draft

    def set(self, name: str, value: object) -> None:
        """Stage a scoped setting until this activation exits or replaces it."""
        if not isinstance(name, str) or not name:
            raise ValueError("setting name must be a non-empty string")
        self._draft.effects.append(
            {"kind": "set_scoped", "payload": _wire_bytes(value), "name": name, "props": {}}
        )


class _StateEffects:
    __slots__ = ("_draft", "_schema", "value")

    def __init__(
        self,
        draft: _Draft,
        schema: _StateSchema | None,
        value: object | None,
    ) -> None:
        self._draft = draft
        self._schema = schema
        self.value = value

    def replace(self, value: object) -> None:
        """Stage one typed durable-state replacement."""
        if self._schema is None:
            raise RegimeContractError("this regime declares no state")
        self._draft.effects.append(
            {
                "kind": "replace_state",
                "payload": self._schema.encode(value),
                "state_revision": self._schema.revision,
                "props": {},
            }
        )


class RegimeContext:
    """Provide read-only event data and transaction-scoped effect writers."""

    __slots__ = ("event", "context", "tool", "settings", "state")

    def __init__(
        self,
        point: Point,
        event: Mapping[str, object],
        state: object | None,
        state_schema: _StateSchema | None,
        draft: _Draft,
    ) -> None:
        self.event = RegimeEvent(point, event)
        self.context = _ContextEffects(draft)
        self.tool = _ToolEffects(draft)
        self.settings = _SettingEffects(draft)
        self.state = _StateEffects(draft, state_schema, state)


class Next:
    """Select at most one event control without invoking a sibling regime."""

    __slots__ = ("_draft", "_point", "_sealed")

    def __init__(self, point: Point, draft: _Draft) -> None:
        self._draft = draft
        self._point = point
        self._sealed = False

    def _select(self, kind: str, **fields: object) -> None:
        if self._sealed:
            raise RegimeContractError("next_ control is already sealed")
        if kind not in _ALLOWED_CONTROLS[self._point]:
            raise RegimeContractError(
                f"control {kind!r} is not available at {self._point.value!r}"
            )
        self._sealed = True
        self._draft.control = {"kind": kind, **fields, "props": {}}

    def retry(self) -> None:
        """Request another model turn instead of settlement."""
        self._select("retry")

    def wait(self, ticket: object) -> None:
        """Park progress behind one durable required-deadline ticket."""
        if ticket is None:
            raise ValueError("wait requires a ticket")
        if isinstance(ticket, Mapping):
            ticket_id = ticket.get("id")
            deadline = ticket.get("deadline_ms")
            reason = ticket.get("reason")
        else:
            ticket_id = getattr(ticket, "id", None)
            deadline = getattr(ticket, "deadline_ms", None)
            reason = getattr(ticket, "reason", None)
        if not isinstance(ticket_id, str) or not ticket_id:
            raise ValueError("wait ticket id must be a non-empty string")
        if (
            isinstance(deadline, bool)
            or not isinstance(deadline, int)
            or deadline < 0
        ):
            raise ValueError("wait ticket deadline_ms must be a non-negative integer")
        self._select(
            "wait",
            wait_ticket=ticket_id,
            wait_deadline_ms=deadline,
            reason=_reason(reason),
        )

    def reject(self, reason: str) -> None:
        """Reject pending work with a durable reason."""
        self._select("reject", reason=_reason(reason))

    def cancel(self, reason: str) -> None:
        """Cancel work already in flight with a durable reason."""
        self._select("cancel", reason=_reason(reason))

    def complete(self) -> None:
        """Complete only this activation successfully."""
        self._select("complete")

    def fail(self, error: object) -> None:
        """Finish this activation with a typed terminal error."""
        if error is None:
            raise ValueError("fail requires an error")
        if isinstance(error, BaseException):
            value: object = {
                "type": f"{type(error).__module__}:{type(error).__qualname__}",
                "message": str(error),
                "args": error.args,
            }
        else:
            value = error
        self._select("fail", error=_wire_bytes(value))


@dataclass(frozen=True, slots=True)
class RegimeRecord:
    """Project one active or resource-queued regime activation."""

    id: str
    regime: str
    extension: str
    status: str


@dataclass(frozen=True, slots=True)
class RegimeHandle:
    """Reference an activation returned by :func:`start`."""

    id: str
    regime: str
    extension: str
    status: str

    async def stop(self) -> bool:
        """Stop this activation through the host CONTROL authority."""
        return await stop(self.id)


def regime(
    id: str,
    *,
    on: Point | Sequence[Point],
    lifetime: RegimeLifetime | str = RegimeLifetime.RUN,
    state: type | None = None,
    when: object | None = None,
    max_steps: int | None = None,
    on_limit: Callable[..., object] | None = None,
    owns: Sequence[str] = (),
    sets: Mapping[str, object] | None = None,
    minimum_duration: Duration | None = None,
    on_failure: OnFailure | str = OnFailure.DEFER,
) -> Callable[[_RegimeTarget], _RegimeTarget]:
    """Declare one isolated regime middleware handler without host I/O."""
    if registry.sealed:
        raise LateRegistration("regime declarations are sealed")
    if not isinstance(id, str) or not id:
        raise ValueError("regime id must be a non-empty string")
    points = _points(on)
    if isinstance(lifetime, str):
        try:
            lifetime = RegimeLifetime(lifetime)
        except ValueError as error:
            raise RegimeContractError(
                f"unknown regime lifetime {lifetime!r}"
            ) from error
    if not isinstance(lifetime, RegimeLifetime):
        raise TypeError("lifetime must be RegimeLifetime or str")
    activation_when = _activation_when(when)
    if state is not None and (
        not isinstance(state, type) or not is_dataclass(state)
    ):
        raise TypeError("regime state must be a dataclass type or None")
    if max_steps is not None and (
        isinstance(max_steps, bool)
        or not isinstance(max_steps, int)
        or max_steps < 1
        or max_steps > 0xFFFF_FFFF
    ):
        raise ValueError("max_steps must be an integer from 1 through 4294967295 or None")
    if on_limit is not None and not callable(on_limit):
        raise TypeError("on_limit must be callable or None")
    if on_limit is not None and max_steps is None:
        raise RegimeContractError("on_limit requires max_steps")
    if on_limit is not None:
        _validate_handler(on_limit, "on_limit")
    owned = _names("owns", owns)
    scoped = _settings(sets)
    if minimum_duration is not None:
        if not isinstance(minimum_duration, Duration):
            raise TypeError("minimum_duration must be Duration or None")
        if minimum_duration.seconds < 0:
            raise ValueError("minimum_duration must not be negative")
    if isinstance(on_failure, str):
        try:
            on_failure = OnFailure(on_failure)
        except ValueError as error:
            raise RegimeContractError(
                f"unknown regime failure behavior {on_failure!r}"
            ) from error
    if not isinstance(on_failure, OnFailure):
        raise TypeError("on_failure must be OnFailure or str")
    state_schema = None
    if state is not None:
        state_schema = _StateSchema(
            f"{state.__module__}.{state.__qualname__}",
            1,
            state,
        )

    def decorate(target: _RegimeTarget) -> _RegimeTarget:
        if registry.sealed:
            raise LateRegistration("regime declarations are sealed")
        if not callable(target):
            raise TypeError("@omp.regime may decorate only a callable or class")
        _validate_handler(target, "regime")
        declaration = RegimeDeclaration(
            id=id,
            points=points,
            target=target,
            lifetime=lifetime,
            state=state_schema,
            when=activation_when,
            max_steps=max_steps,
            on_limit=on_limit,
            owns=owned,
            sets=scoped,
            minimum_duration=minimum_duration,
            on_failure=on_failure,
        )
        try:
            registry.register_regime(id, declaration)
        except DeclarationSealed as error:
            raise LateRegistration("regime declarations are sealed") from error
        prior = tuple(getattr(target, "__omp_regimes__", ()))
        setattr(target, "__omp_regimes__", prior + (declaration,))
        return target

    return decorate


def _sealed_regime_declaration(host_generation: int) -> dict[str, object]:
    """Project the authoritative frozen regime table for host publication."""
    if not registry.sealed:
        raise RuntimeError("regime declarations publish only after FREEZE")
    if (
        isinstance(host_generation, bool)
        or not isinstance(host_generation, int)
        or host_generation < 1
    ):
        raise ValueError("regime declaration generation must be positive")
    manifests: list[dict[str, object]] = []
    for declaration in registry.snapshot().regimes:
        manifests.append(
            {
                "id": declaration.id,
                "revision": declaration.revision,
                "points": [point.value for point in declaration.points],
                "precedence": declaration.precedence,
                "lifetime": declaration.lifetime.value,
                "max_steps": declaration.max_steps,
                "committed_step_interval_ms": None,
                "has_on_limit": declaration.on_limit is not None,
                "state_family": (
                    None if declaration.state is None else declaration.state.family
                ),
                "state_revision": (
                    None if declaration.state is None else declaration.state.revision
                ),
                "when": (
                    b"" if declaration.when is None else _wire_bytes(declaration.when)
                ),
                "owns": list(declaration.owns),
                "sets": _wire_bytes(dict(declaration.sets)),
                "minimum_duration_ms": (
                    None
                    if declaration.minimum_duration is None
                    else round(declaration.minimum_duration.seconds * 1000)
                ),
                "on_failure": declaration.on_failure.value,
                "props": {},
            }
        )
    return {
        "extension_id": registry.extension_id or "",
        "generation": host_generation,
        "api_level": 1,
        "table_revision": 1,
        "manifests": manifests,
        "point_table": list(_POINT_TABLE),
        "control_table": list(_CONTROL_TABLE),
        "effect_table": list(_EFFECT_TABLE),
        "props": {},
    }


async def start(
    regime: str,
    *,
    state: object | None = None,
    queue: bool = False,
) -> RegimeHandle:
    """Start one declared regime, optionally queueing for owned resources."""
    declaration = _regime_declaration(regime)
    if not isinstance(queue, bool):
        raise TypeError("regime queue must be bool")
    encoded_state: bytes = b""
    state_revision: int | None = None
    if state is not None:
        if declaration.state is None:
            raise RegimeContractError(f"regime {regime!r} declares no state")
        encoded_state = declaration.state.encode(state)
        state_revision = declaration.state.revision
    from . import _control_request

    response = await _control_request(
        "omp.regimes.start",
        regime_id=regime,
        state=encoded_state,
        state_revision=state_revision,
        queue=queue,
    )
    record = _record(response)
    return RegimeHandle(record.id, record.regime, record.extension, record.status)


async def active(*, extension: str | None = None) -> tuple[RegimeRecord, ...]:
    """List own activations, or another extension with ``regimes.read``."""
    if extension is not None and (
        not isinstance(extension, str) or not extension
    ):
        raise ValueError("regime extension must be a non-empty string or None")
    from . import _control_request

    rows = await _control_request("omp.regimes.active", extension=extension)
    if not isinstance(rows, Sequence) or isinstance(rows, (str, bytes)):
        raise StateDecodeError("regime active response must be a sequence")
    return tuple(_record(row) for row in rows)


async def stop(activation_id: str) -> bool:
    """Stop one activation owned by the calling extension."""
    if not isinstance(activation_id, str) or not activation_id:
        raise ValueError("activation id must be a non-empty string")
    from . import _control_request

    result = await _control_request(
        "omp.regimes.stop", activation_id=activation_id
    )
    if not isinstance(result, bool):
        raise TypeError("omp.regimes.stop host result must be bool")
    return result


async def dispatch_regime_start(
    regime_id: str,
    activation_id: str,
    state: bytes = b"",
    *,
    regime_revision: int = 1,
    state_revision: int | None = None,
    deadline_ms: int | None = None,
    props: Mapping[str, object] | None = None,
) -> None:
    """Initialize one activation-local class handler for a worker start envelope."""
    del deadline_ms, props
    declaration = _regime_declaration(regime_id)
    _validate_revision(declaration, regime_revision)
    if not activation_id:
        raise RegimeContractError("regime start requires an activation id")
    if declaration.state is None and state:
        raise RegimeContractError(f"regime {regime_id!r} declares no state")
    if declaration.state is not None and state:
        declaration.state.decode(state, state_revision)
    if isinstance(declaration.target, type):
        key = (regime_id, activation_id)
        if key in _REGIME_INSTANCES:
            raise RegimeContractError(
                f"regime activation is already started: {activation_id!r}"
            )
        _REGIME_INSTANCES[key] = declaration.target()


async def dispatch_regime_stop(
    regime_id: str,
    activation_id: str,
    *,
    regime_revision: int = 1,
    reason: str | None = None,
    deadline_ms: int | None = None,
    props: Mapping[str, object] | None = None,
) -> None:
    """Release one activation-local class handler for a worker stop envelope."""
    del reason, deadline_ms, props
    declaration = _regime_declaration(regime_id)
    _validate_revision(declaration, regime_revision)
    if not activation_id:
        raise RegimeContractError("regime stop requires an activation id")
    _REGIME_INSTANCES.pop((regime_id, activation_id), None)


async def dispatch_regime_apply(
    regime_id: str,
    point: Point | str,
    event_payload: bytes,
    state: bytes = b"",
    activation_id: str = "",
    regime_revision: int = 1,
    event_revision: int = 1,
    committed_steps: int = 0,
    deadline_ms: int | None = None,
    state_revision: int | None = None,
    limit_handler: bool = False,
    props: Mapping[str, object] | None = None,
) -> dict[str, object]:
    """Run one isolated handler and return its optional control and ordered effects."""
    del committed_steps, deadline_ms, props
    declaration = _regime_declaration(regime_id)
    point = Point(point)
    if point not in declaration.points:
        raise RegimeContractError(
            f"regime {regime_id!r} is not subscribed to point {point.value!r}"
        )
    _validate_revision(declaration, regime_revision)
    if event_revision != 1:
        raise RegimeContractError(
            f"unsupported regime event revision {event_revision}"
        )
    if not activation_id:
        raise RegimeContractError("regime apply requires an activation id")
    try:
        decoded_event = (
            json.loads(event_payload.decode("utf-8")) if event_payload else {}
        )
    except (AttributeError, UnicodeDecodeError, ValueError) as error:
        raise RegimeContractError("regime event payload is malformed") from error
    if not isinstance(decoded_event, Mapping):
        raise RegimeContractError("regime event payload must be an object")
    decoded_state = None
    if declaration.state is not None and state:
        if state_revision is not None and (
            isinstance(state_revision, bool) or not isinstance(state_revision, int)
        ):
            raise StateDecodeError("regime state revision must be an integer")
        decoded_state = declaration.state.decode(state, state_revision)
    elif state:
        raise RegimeContractError(f"regime {regime_id!r} declares no state")
    draft = _Draft()
    ctx = RegimeContext(point, decoded_event, decoded_state, declaration.state, draft)
    next_ = Next(point, draft)
    callback = _callback(declaration, activation_id)
    if limit_handler:
        if declaration.on_limit is None:
            raise RegimeContractError(f"regime {regime_id!r} has no on_limit handler")
        callback = declaration.on_limit
    result = callback(ctx, next_)
    if inspect.isawaitable(result):
        result = await result
    if result is not None:
        raise RegimeContractError(
            "regime handlers return only the result of next_ control methods or None"
        )
    return {
        "activation_id": activation_id,
        "regime_revision": regime_revision,
        "event_revision": event_revision,
        "control": draft.control,
        "effects": draft.effects,
        "props": {},
    }


def _callback(
    declaration: RegimeDeclaration,
    activation_id: str,
) -> Callable[..., object]:
    target = declaration.target
    if not isinstance(target, type):
        return target
    key = (declaration.id, activation_id)
    instance = _REGIME_INSTANCES.get(key)
    if instance is None:
        instance = target()
        _REGIME_INSTANCES[key] = instance
    callback = getattr(instance, "apply", None)
    if not callable(callback):
        raise RegimeContractError(
            f"regime {declaration.id!r} class has no callable apply handler"
        )
    return callback


def _record(row: object) -> RegimeRecord:
    if not isinstance(row, Mapping):
        raise StateDecodeError("regime activation row must be a mapping")
    activation_id = row.get("id")
    regime_id = row.get("regime")
    extension = row.get("extension")
    status = row.get("status")
    if not isinstance(activation_id, str) or not activation_id:
        raise StateDecodeError("regime activation id must be a non-empty string")
    if not isinstance(regime_id, str) or not regime_id:
        raise StateDecodeError("regime activation regime must be a non-empty string")
    if not isinstance(extension, str) or not extension:
        raise StateDecodeError("regime activation extension must be a non-empty string")
    if status not in {"active", "queued"}:
        raise StateDecodeError("regime activation status must be 'active' or 'queued'")
    return RegimeRecord(activation_id, regime_id, extension, status)


def _regime_declaration(regime_id: str) -> RegimeDeclaration:
    for declaration in registry.snapshot().regimes:
        if declaration.id == regime_id:
            return declaration
    raise LookupError(f"regime declaration is not registered: {regime_id!r}")


def _validate_revision(
    declaration: RegimeDeclaration,
    revision: int,
) -> None:
    if revision != declaration.revision:
        raise RegimeContractError(
            f"regime revision {revision} is stale; expected {declaration.revision}"
        )


def _points(value: Point | Sequence[Point]) -> tuple[Point, ...]:
    if isinstance(value, Point):
        points = (value,)
    else:
        if isinstance(value, (str, bytes)):
            raise TypeError("on must be Point or a sequence of Point members")
        points = tuple(value)
    if not points or any(not isinstance(point, Point) for point in points):
        raise TypeError("on must contain at least one Point member")
    if len(set(points)) != len(points):
        raise RegimeContractError("regime points must be unique")
    return points


def _activation_when(value: object | None) -> Mapping[str, object] | None:
    if value is None:
        return None
    if not isinstance(value, Mapping):
        raise TypeError("when must be an omp.when condition or None")
    allowed = {
        "point",
        "invocation_id",
        "stream_contains",
        "delivered",
        "checkpoint_active",
    }
    unknown = set(value) - allowed
    if unknown:
        raise RegimeContractError(
            f"when contains unknown fields: {', '.join(sorted(map(str, unknown)))}"
        )
    try:
        point = Point(value["point"])
    except (KeyError, TypeError, ValueError) as error:
        raise RegimeContractError("when.point must name one fixed regime event") from error
    normalized: dict[str, object] = {"point": point.value}
    for name in ("invocation_id", "stream_contains"):
        item = value.get(name)
        if item is not None and (not isinstance(item, str) or not item):
            raise RegimeContractError(f"when.{name} must be a non-empty string or None")
        if item is not None:
            normalized[name] = item
    for name in ("delivered", "checkpoint_active"):
        item = value.get(name)
        if item is not None and not isinstance(item, bool):
            raise RegimeContractError(f"when.{name} must be bool or None")
        if item is not None:
            normalized[name] = item
    return MappingProxyType(normalized)


def _validate_handler(target: Callable[..., object] | type, label: str) -> None:
    callback = getattr(target, "apply", None) if isinstance(target, type) else target
    if not callable(callback):
        raise RegimeContractError(
            f"{label} class must define a callable apply(ctx, next_) handler"
        )
    try:
        inspect.signature(callback).bind(*((None, None, None) if isinstance(target, type) else (None, None)))
    except (TypeError, ValueError) as error:
        raise RegimeContractError(
            f"{label} handler must accept exactly ctx and next_"
        ) from error


def _names(name: str, values: Sequence[str]) -> tuple[str, ...]:
    if isinstance(values, (str, bytes)):
        raise TypeError(f"{name} must be a sequence of strings")
    normalized = tuple(values)
    if any(not isinstance(value, str) or not value for value in normalized):
        raise TypeError(f"{name} must contain only non-empty strings")
    if len(set(normalized)) != len(normalized):
        raise RegimeContractError(f"{name} must be unique")
    return normalized


def _settings(values: Mapping[str, object] | None) -> Mapping[str, object]:
    if values is None:
        return MappingProxyType({})
    if not isinstance(values, Mapping):
        raise TypeError("sets must be a mapping or None")
    normalized = dict(values)
    if any(not isinstance(name, str) or not name for name in normalized):
        raise TypeError("sets keys must be non-empty strings")
    return MappingProxyType(normalized)


def _reason(value: str) -> str:
    if not isinstance(value, str) or not value:
        raise ValueError("control reason must be a non-empty string")
    return value


def _context_item(value: object) -> Mapping[str, object]:
    encoded = _wire_value(value)
    if not isinstance(encoded, Mapping):
        raise TypeError("context.append items must be canonical thread items")
    kinds = sum(name in encoded for name in ("message", "tool_call", "tool_result"))
    if kinds != 1:
        raise RegimeContractError(
            "context item must contain exactly one of message, tool_call, or tool_result"
        )
    return encoded


def _wire_value(value: object) -> object:
    from .provider import _wire_value as encode

    return encode(value)


def _wire_bytes(value: object) -> bytes:
    return json.dumps(
        _wire_value(value), sort_keys=True, separators=(",", ":")
    ).encode("utf-8")


__all__ = (
    "ADMISSION",
    "BATCH",
    "CONTEXT",
    "IDLE",
    "PRE_MODEL",
    "SETTLE",
    "STREAM",
    "TOOL_CHOICE",
    "TURN_END",
    "Next",
    "Point",
    "RegimeContext",
    "RegimeContractError",
    "RegimeEvent",
    "RegimeHandle",
    "RegimeLifetime",
    "RegimeRecord",
    "StateDecodeError",
    "StateSchemaMismatch",
    "active",
    "regime",
    "start",
    "stop",
    "user_text",
    "when",
)
