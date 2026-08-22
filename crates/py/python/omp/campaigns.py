"""Pure-Python campaign declarations and closed reaction vocabulary.

Importing this module records declarations only.  Engagement and CONTROL
reaction dispatch land with the host-side campaign runtime.
"""

from __future__ import annotations
import inspect
import json
import os

from collections.abc import Callable, Mapping, Sequence
from dataclasses import asdict, dataclass, is_dataclass
from enum import StrEnum
from typing import Any, Final, TypeAlias, TypeVar

from _omp import Duration, OmpError

from ._errors import DeclarationSealed
from ._registry import registry
from ._verdicts import Done
from .hooks import LateRegistration, OnFailure

_CampaignTarget = TypeVar("_CampaignTarget", bound=Callable[..., object] | type)
_CAMPAIGN_INSTANCES: dict[str, object] = {}


class CampaignContractError(OmpError, ValueError):
    """A campaign declaration violates the frozen campaign contract."""

class ModeClaimRequired(CampaignContractError):
    """A Session campaign binds a mode surface without owning or composing it."""

    def __init__(self, campaign: str, binding: str) -> None:
        self.campaign = campaign
        self.binding = binding
        super().__init__(
            f"session campaign {campaign!r} binds {binding!r} without "
            "claiming 'mode' or declaring composes=True"
        )

class StateSchemaMismatch(OmpError, ValueError):
    """A durable campaign state blob names a different ``family@rev``."""

    def __init__(self, expected: str, actual: str) -> None:
        self.expected = expected
        self.actual = actual
        super().__init__(
            f"campaign state schema mismatch: expected {expected!r}, got {actual!r}"
        )


class StateDecodeError(OmpError, ValueError):
    """A durable campaign state blob cannot rebuild its declared dataclass."""


class Point(StrEnum):
    """Name one decision point in the closed agent loop."""

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
TOOL_CHOICE: Final[Point] = Point.TOOL_CHOICE
PRE_MODEL: Final[Point] = Point.PRE_MODEL
STREAM: Final[Point] = Point.STREAM
ADMISSION: Final[Point] = Point.ADMISSION
BATCH: Final[Point] = Point.BATCH
TURN_END: Final[Point] = Point.TURN_END
SETTLE: Final[Point] = Point.SETTLE
IDLE: Final[Point] = Point.IDLE


class CampaignScope(StrEnum):
    """Bound the lifetime of one campaign engagement."""

    TURN = "turn"
    RUN = "run"
    SESSION = "session"


class Exhaust(StrEnum):
    """Select the terminal action when a campaign ladder is exhausted."""

    SETTLE = "settle"
    FAULT = "fault"


@dataclass(frozen=True, slots=True)
class Ladder:
    """Declare the finite bounds for one campaign escalation ladder."""

    max_engagements: int
    max_turns: int | None = None
    min_interval: Duration | None = None

    def __post_init__(self) -> None:
        if (
            isinstance(self.max_engagements, bool)
            or not isinstance(self.max_engagements, int)
            or self.max_engagements < 1
        ):
            raise ValueError("max_engagements must be a positive integer")
        if self.max_turns is not None and (
            isinstance(self.max_turns, bool)
            or not isinstance(self.max_turns, int)
            or self.max_turns < 1
        ):
            raise ValueError("max_turns must be a positive integer or None")
        if self.min_interval is not None and not isinstance(self.min_interval, Duration):
            raise TypeError("min_interval must be Duration or None")


@dataclass(frozen=True, slots=True)
class StateVersion:
    """Identify the journal schema of a campaign state dataclass."""

    family: str
    rev: int
    state_type: type

    def __post_init__(self) -> None:
        if not isinstance(self.family, str) or not self.family or "@" in self.family:
            raise ValueError("state family must be a non-empty string without '@'")
        if isinstance(self.rev, bool) or not isinstance(self.rev, int) or self.rev < 1:
            raise ValueError("state rev must be a positive integer")
        if not isinstance(self.state_type, type) or not is_dataclass(self.state_type):
            raise TypeError("campaign state must be a dataclass type")

    @property
    def wire_name(self) -> str:
        """Return the durable ``family@rev`` schema identifier."""

        return f"{self.family}@{self.rev}"
    def encode(self, value: object) -> bytes:
        """Encode one typed state value into its durable JSON envelope."""

        if not isinstance(value, self.state_type):
            raise TypeError(f"state must be {self.state_type.__qualname__}")
        return json.dumps(
            {"family_rev": self.wire_name, "state": asdict(value)},
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")

    def decode(self, payload: bytes | str) -> object:
        """Decode and schema-check one durable JSON envelope."""

        try:
            raw = payload.decode("utf-8") if isinstance(payload, bytes) else payload
            envelope = json.loads(raw)
            actual = envelope["family_rev"]
            values = envelope["state"]
        except (AttributeError, KeyError, TypeError, UnicodeDecodeError, ValueError) as error:
            raise StateDecodeError("campaign state payload is malformed") from error
        if actual != self.wire_name:
            raise StateSchemaMismatch(self.wire_name, str(actual))
        if not isinstance(values, dict):
            raise StateDecodeError("campaign state fields must be an object")
        try:
            return self.state_type(**values)
        except (TypeError, ValueError) as error:
            raise StateDecodeError("campaign state fields do not match the dataclass") from error


class Verdict:
    """Marker base for the closed campaign reaction vocabulary."""

    __slots__ = ()


@dataclass(frozen=True, slots=True)
class Pass(Verdict):
    """Make no change at this decision point."""


@dataclass(frozen=True, slots=True, init=False)
class Inject(Verdict):
    """Add messages or interrupts at an eligible drain point."""

    items: tuple[object, ...]
    at: str
    via: str
    once: bool

    def __init__(
        self,
        *items: object,
        at: str = "turn-boundary",
        via: str = "context",
        once: bool = False,
    ) -> None:
        if not items:
            raise ValueError("Inject requires at least one item")
        if not isinstance(at, str) or not at:
            raise TypeError("at must be a non-empty string")
        if via not in {"context", "aside", "preserve"}:
            raise ValueError("via must be 'context', 'aside', or 'preserve'")
        if not isinstance(once, bool):
            raise TypeError("once must be bool")
        object.__setattr__(self, "items", tuple(items))
        object.__setattr__(self, "at", at)
        object.__setattr__(self, "via", via)
        object.__setattr__(self, "once", once)


@dataclass(frozen=True, slots=True)
class Patch(Verdict):
    """Rewrite the wire context through an ordered patch payload."""

    patch: object


@dataclass(frozen=True, slots=True)
class Hold(Verdict):
    """Park progress behind a durable ticket or required deadline."""

    ticket: object | None = None
    until: object | None = None

    def __post_init__(self) -> None:
        if self.ticket is None and self.until is None:
            raise CampaignContractError("Hold requires a ticket or deadline")


@dataclass(frozen=True, slots=True)
class EngageRequest:
    """Atomic hook hand-off request to engage a campaign after its fold."""
 
    campaign: str
    state: object | None = None
    queue: bool = False
 
 
@dataclass(frozen=True, slots=True)
class Deny(Verdict):
    """Veto the current attempt without latching."""

    reason: str
    fatal: bool = False
    code: str | None = None
    engage: str | EngageRequest | None = None


@dataclass(frozen=True, slots=True)
class Continue(Verdict):
    """Veto settlement and begin another turn."""

    inject: object | None = None


@dataclass(frozen=True, slots=True)
class Force(Verdict):
    """Claim the exclusive tool-choice slot for a named tool."""

    tool: str
    args: Mapping[str, Any] | None = None
    satisfies: Callable[[object], bool] | None = None

    def __post_init__(self) -> None:
        if not isinstance(self.tool, str) or not self.tool:
            raise ValueError("tool must be a non-empty string")


@dataclass(frozen=True, slots=True)
class Cut(Verdict):
    """Abort the current stream or batch with an optional expiry."""

    reason: str
    expires: object | None = None


@dataclass(frozen=True, slots=True)
class Bind(Verdict):
    """Push a scoped value onto a named binding slot."""

    slot: str
    value: object
    scope: str = "engagement"

    def __post_init__(self) -> None:
        if not isinstance(self.slot, str) or not self.slot:
            raise ValueError("slot must be a non-empty string")
        if self.scope not in {"engagement", "turn", "branch"}:
            raise ValueError("scope must be 'engagement', 'turn', or 'branch'")


@dataclass(frozen=True, slots=True)
class Exhausted(Verdict):
    """Complete a campaign through its configured exhaustion policy."""

    reason: str | None = None


@dataclass(frozen=True, slots=True)
class Escalate(Verdict):
    """Advance a campaign to its next ladder rung."""

    reason: str | None = None


CampaignVerdict: TypeAlias = (
    Pass
    | Inject
    | Patch
    | Hold
    | Deny
    | Continue
    | Force
    | Cut
    | Bind
    | Done
    | Exhausted
    | Escalate
)
"""The closed campaign reaction vocabulary."""


POINT_TABLE: Final[tuple[str, ...]] = tuple(point.value for point in Point)
"""Frozen point spelling table shared with ``toolhost/v1``."""

VERDICT_TABLE: Final[tuple[str, ...]] = (
    "pass",
    "inject",
    "patch",
    "hold",
    "deny",
    "continue",
    "force",
    "cut",
    "bind",
    "done",
    "exhausted",
    "escalate",
)
"""Frozen verdict spelling table shared with ``toolhost/v1``."""


@dataclass(frozen=True, slots=True)
class CampaignDeclaration:
    """One import-time campaign declaration sealed at FREEZE."""

    id: str
    rev: int
    points: tuple[Point, ...]
    target: Callable[..., object] | type
    ladder: Ladder | None
    exhaust: Exhaust | Verdict | Done[Any]
    scope: CampaignScope
    state: StateVersion | None
    policy: object | None
    when: object | None
    on_failure: OnFailure
    claims: tuple[str, ...]
    binds: tuple[str, ...]
    composes: bool


def campaign(
    id: str,
    *,
    at: Point | Sequence[Point],
    rev: int = 1,
    ladder: Ladder | None = None,
    exhaust: Exhaust | Verdict | Done[Any] = Exhaust.SETTLE,
    scope: CampaignScope | str = CampaignScope.RUN,
    state: type | None = None,
    state_family: str | None = None,
    state_rev: int = 1,
    policy: object | None = None,
    when: object | None = None,
    on_failure: OnFailure | str = OnFailure.DEFER,
    claims: Sequence[str] = (),
    binds: Sequence[str] = (),
    composes: bool = False,
) -> Callable[[_CampaignTarget], _CampaignTarget]:
    """Declare one campaign without performing host I/O."""

    if registry.sealed:
        raise LateRegistration("campaign declarations are sealed")
    if not isinstance(id, str) or not id:
        raise ValueError("campaign id must be a non-empty string")
    if isinstance(rev, bool) or not isinstance(rev, int) or rev < 1:
        raise ValueError("campaign rev must be a positive integer")
    if isinstance(at, Point):
        points = (at,)
    else:
        if isinstance(at, (str, bytes)):
            raise TypeError("at must be Point or a sequence of Point members")
        points = tuple(at)
    if not points or any(not isinstance(point, Point) for point in points):
        raise TypeError("at must contain at least one Point member")
    if len(set(points)) != len(points):
        raise CampaignContractError("campaign points must be unique")
    if Point.STREAM in points:
        raise CampaignContractError("extension campaigns cannot subscribe to STREAM in v1")
    if ladder is not None and not isinstance(ladder, Ladder):
        raise TypeError("ladder must be Ladder or None")
    if isinstance(exhaust, str):
        try:
            exhaust = Exhaust(exhaust)
        except ValueError as error:
            raise CampaignContractError(f"unknown exhaust policy {exhaust!r}") from error
    if not isinstance(exhaust, (Exhaust, Verdict, Done)):
        raise TypeError("exhaust must be Exhaust or Verdict")
    if isinstance(scope, str):
        try:
            scope = CampaignScope(scope)
        except ValueError as error:
            raise CampaignContractError(f"unknown campaign scope {scope!r}") from error
    if not isinstance(scope, CampaignScope):
        raise TypeError("scope must be CampaignScope or str")
    if isinstance(on_failure, str):
        try:
            on_failure = OnFailure(on_failure)
        except ValueError as error:
            raise CampaignContractError(
                f"unknown campaign failure policy {on_failure!r}"
            ) from error
    if not isinstance(on_failure, OnFailure):
        raise TypeError("on_failure must be OnFailure or str")
    normalized_claims = _declaration_names("claims", claims)
    normalized_binds = _declaration_names("binds", binds)
    if not isinstance(composes, bool):
        raise TypeError("composes must be bool")
    mode_binding = next(
        (
            binding
            for binding in normalized_binds
            if binding.casefold() in {"toolset", "model"}
        ),
        None,
    )
    if (
        scope is CampaignScope.SESSION
        and mode_binding is not None
        and "mode" not in normalized_claims
        and not composes
    ):
        raise ModeClaimRequired(id, mode_binding)
    state_version = None
    if state is not None:
        family = state_family or f"{state.__module__}.{state.__qualname__}"
        state_version = StateVersion(family, state_rev, state)
    elif state_family is not None or state_rev != 1:
        raise CampaignContractError("state_family and state_rev require state")

    def decorate(target: _CampaignTarget) -> _CampaignTarget:
        if registry.sealed:
            raise LateRegistration("campaign declarations are sealed")
        if not callable(target):
            raise TypeError("@omp.campaign may decorate only a callable or class")
        declaration = CampaignDeclaration(
            id,
            rev,
            points,
            target,
            ladder,
            exhaust,
            scope,
            state_version,
            policy,
            when,
            on_failure,
            normalized_claims,
            normalized_binds,
            composes,
        )
        try:
            registry.register_campaign(id, declaration)
        except DeclarationSealed as error:
            raise LateRegistration("campaign declarations are sealed") from error
        prior = tuple(getattr(target, "__omp_campaigns__", ()))
        setattr(target, "__omp_campaigns__", prior + (declaration,))
        return target

    return decorate


def _sealed_campaign_declaration(
    host_generation: int,
) -> dict[str, object]:
    """Project the authoritative frozen campaign table for host publication."""
    if not registry.sealed:
        raise RuntimeError("campaign declarations publish only after FREEZE")
    if (
        isinstance(host_generation, bool)
        or not isinstance(host_generation, int)
        or host_generation < 1
    ):
        raise ValueError("campaign declaration generation must be positive")
    from .provider import _wire_value

    manifests: list[dict[str, object]] = []
    for declaration in registry.snapshot().campaigns:
        if isinstance(declaration.exhaust, Exhaust):
            exhaust: object = declaration.exhaust.value
        else:
            exhaust = {
                "verdict": _verdict_name(declaration.exhaust),
                "payload": json.loads(_verdict_payload(declaration.exhaust)),
            }
        manifests.append({
            "id": declaration.id,
            "rev": declaration.rev,
            "points": [point.value for point in declaration.points],
            "scope": declaration.scope.value,
            "exhaust": exhaust,
            "state_family": (
                None if declaration.state is None else declaration.state.family
            ),
            "state_rev": (
                None if declaration.state is None else declaration.state.rev
            ),
            "ladder": _wire_value(declaration.ladder),
            "policy": _wire_value(declaration.policy),
            "when": _wire_value(declaration.when),
            "on_failure": declaration.on_failure.value,
            "claims": list(declaration.claims),
            "binds": list(declaration.binds),
            "composes": declaration.composes,
        })
    return {
        "generation": host_generation,
        "table_rev": 1,
        "point_table": list(POINT_TABLE),
        "verdict_table": list(VERDICT_TABLE),
        "manifests": manifests,
    }


@dataclass(frozen=True, slots=True)
class ActiveCampaign:
    """One active or queued engagement projected by Core."""

    id: str
    campaign: str
    extension: str
    state: object | None
    queued: bool = False


async def engage(
    campaign: str,
    *,
    state: object | None = None,
    queue: bool = False,
) -> ActiveCampaign:
    """Engage one declared campaign through the host CONTROL authority."""

    declaration = _campaign_declaration(campaign)
    if not isinstance(queue, bool):
        raise TypeError("campaign queue must be bool")
    payload = None
    if state is not None:
        if declaration.state is None:
            raise CampaignContractError(f"campaign {campaign!r} declares no state")
        payload = declaration.state.encode(state).decode("utf-8")
    from . import _control_request

    response = await _control_request(
        "omp.campaigns.engage",
        campaign=campaign,
        state=payload,
        queue=queue,
    )
    return _active_campaign(response, declaration)


async def active(*, extension: str | None = None) -> tuple[ActiveCampaign, ...]:
    """List own engagements, or another extension with ``campaigns.read``."""

    if extension is not None and (
        not isinstance(extension, str) or not extension
    ):
        raise ValueError("campaign extension must be a non-empty string or None")
    from . import _control_request

    rows = await _control_request("omp.campaigns.active", extension=extension)
    if not isinstance(rows, Sequence) or isinstance(rows, (str, bytes)):
        raise StateDecodeError("campaign active response must be a sequence")
    return tuple(_active_campaign(row) for row in rows)


async def disengage(engagement: str) -> bool:
    """Disengage one engagement owned by the calling extension."""

    if not isinstance(engagement, str) or not engagement:
        raise ValueError("engagement must be a non-empty string")
    from . import _control_request

    result = await _control_request("omp.campaigns.disengage", engagement=engagement)
    if not isinstance(result, bool):
        raise TypeError("omp.campaigns.disengage host result must be bool")
    return result


async def dispatch_campaign_react(
    campaign: str,
    point: Point | str,
    event_payload: bytes,
    state_payload: bytes = b"",
    *,
    engagement_id: str = "",
    campaign_rev: int | None = None,
    event_rev: int = 1,
    host_generation: int | None = None,
    session_generation: int | None = None,
) -> dict[str, object]:
    """Run one generation-fenced handler and return a typed wire reaction."""

    declaration = _campaign_declaration(campaign)
    point = Point(point)
    if point not in declaration.points:
        raise CampaignContractError(
            f"campaign {campaign!r} is not subscribed to point {point.value!r}"
        )
    if campaign_rev is None:
        campaign_rev = declaration.rev
    if campaign_rev != declaration.rev:
        raise CampaignContractError(
            f"campaign revision {campaign_rev} is stale; expected {declaration.rev}"
        )
    if event_rev != 1:
        raise CampaignContractError(f"unsupported campaign event revision {event_rev}")
    _validate_dispatch_generation(
        "OMP_EXT_HOST_GENERATION", host_generation, "host"
    )
    _validate_dispatch_generation(
        "OMP_EXT_SESSION_GENERATION", session_generation, "session"
    )
    if host_generation is not None and not engagement_id:
        raise CampaignContractError("host campaign dispatch requires an engagement id")
    try:
        event = json.loads(event_payload.decode("utf-8")) if event_payload else {}
    except (AttributeError, UnicodeDecodeError, ValueError) as error:
        raise CampaignContractError("campaign event payload is malformed") from error
    state = None
    if declaration.state is not None and state_payload:
        state = declaration.state.decode(state_payload)
    target = declaration.target
    callback = target
    if isinstance(target, type):
        instance = _CAMPAIGN_INSTANCES.get(campaign)
        if instance is None:
            instance = target()
            _CAMPAIGN_INSTANCES[campaign] = instance
        callback = getattr(instance, "react", instance)
    if not callable(callback):
        raise CampaignContractError(
            f"campaign {campaign!r} implementation has no callable react handler"
        )
    result = callback(event, state) if declaration.state is not None else callback(event)
    if inspect.isawaitable(result):
        result = await result
    new_state = state
    if (
        isinstance(result, tuple)
        and len(result) == 2
        and isinstance(result[0], (Verdict, Done))
    ):
        if declaration.state is None:
            raise CampaignContractError(
                "stateless campaign handler cannot return replacement state"
            )
        result, new_state = result
    if not isinstance(result, (Verdict, Done)):
        raise CampaignContractError("campaign handler returned no campaign verdict")
    state_bytes = b""
    if declaration.state is not None and new_state is not None:
        state_bytes = declaration.state.encode(new_state)
    return {
        "engagement_id": engagement_id,
        "campaign_rev": campaign_rev,
        "point": point.value,
        "verdict": _verdict_name(result),
        "verdict_payload": _verdict_payload(result),
        "new_state": state_bytes,
    }


def _validate_dispatch_generation(
    variable: str,
    actual: int | None,
    label: str,
) -> None:
    expected_value = os.environ.get(variable)
    if expected_value is None:
        if actual is not None and (
            isinstance(actual, bool) or not isinstance(actual, int) or actual < 1
        ):
            raise CampaignContractError(
                f"campaign {label} generation must be a positive integer"
            )
        return
    try:
        expected = int(expected_value)
    except ValueError as error:
        raise CampaignContractError(
            f"worker {label} generation is malformed"
        ) from error
    if actual != expected:
        raise CampaignContractError(
            f"stale campaign {label} generation {actual!r}; expected {expected}"
        )


def _campaign_declaration(campaign: str) -> CampaignDeclaration:
    for declaration in registry.snapshot().campaigns:
        if declaration.id == campaign:
            return declaration
    raise LookupError(f"campaign declaration is not registered: {campaign!r}")


def _active_campaign(
    row: object,
    declaration: CampaignDeclaration | None = None,
) -> ActiveCampaign:
    if not isinstance(row, Mapping):
        raise StateDecodeError("campaign engagement row must be a mapping")
    campaign = str(row.get("campaign", declaration.id if declaration else ""))
    if declaration is None:
        try:
            declaration = _campaign_declaration(campaign)
        except LookupError:
            declaration = None
    state: object | None = row.get("state")
    if declaration is not None and declaration.state is not None and isinstance(state, str):
        state = declaration.state.decode(state)
    engagement_id = row.get("id")
    extension = row.get("extension")
    queued = row.get("queued", False)
    if not isinstance(engagement_id, str) or not engagement_id:
        raise StateDecodeError("campaign engagement id must be a non-empty string")
    if not campaign:
        raise StateDecodeError("campaign engagement campaign must be non-empty")
    if not isinstance(extension, str) or not extension:
        raise StateDecodeError("campaign engagement extension must be non-empty")
    if not isinstance(queued, bool):
        raise StateDecodeError("campaign engagement queued must be bool")

    return ActiveCampaign(
        id=engagement_id,
        campaign=campaign,
        extension=extension,
        state=state,
        queued=queued,
    )


def _verdict_name(verdict: Verdict | Done[Any]) -> str:
    if isinstance(verdict, Done):
        return "done"
    return {
        Pass: "pass",
        Inject: "inject",
        Patch: "patch",
        Hold: "hold",
        Deny: "deny",
        Continue: "continue",
        Force: "force",
        Cut: "cut",
        Bind: "bind",
        Exhausted: "exhausted",
        Escalate: "escalate",
    }[type(verdict)]


def _verdict_payload(verdict: Verdict | Done[Any]) -> bytes:
    value = asdict(verdict) if is_dataclass(verdict) else {}
    from .provider import _wire_value

    return json.dumps(
        _wire_value(value), sort_keys=True, separators=(",", ":")
    ).encode()


def _declaration_names(name: str, values: Sequence[str]) -> tuple[str, ...]:
    if isinstance(values, (str, bytes)):
        raise TypeError(f"{name} must be a sequence of strings")
    normalized = tuple(values)
    if any(not isinstance(value, str) or not value for value in normalized):
        raise TypeError(f"{name} must contain only non-empty strings")
    if len(set(normalized)) != len(normalized):
        raise CampaignContractError(f"{name} must be unique")
    return normalized


__all__ = (
    "ADMISSION",
    "BATCH",
    "CONTEXT",
    "IDLE",
    "POINT_TABLE",
    "PRE_MODEL",
    "SETTLE",
    "STREAM",
    "TOOL_CHOICE",
    "TURN_END",
    "VERDICT_TABLE",
    "Bind",
    "Continue",
    "Cut",    "ActiveCampaign",

    "Deny",
    "Escalate",
    "Exhaust",
    "Exhausted",
    "Force",
    "Hold",
    "Inject",
    "Ladder",
    "LateRegistration",
    "ModeClaimRequired",
    "CampaignContractError",
    "CampaignDeclaration",
    "CampaignScope",
    "CampaignVerdict",
    "Pass",
    "Patch",
    "Point",
    "Done",    "EngageRequest",

    "StateVersion",
    "StateDecodeError",
    "StateSchemaMismatch",
    "Verdict",
    "active",
    "campaign",
    "disengage",
    "dispatch_campaign_react",
    "engage",
)
