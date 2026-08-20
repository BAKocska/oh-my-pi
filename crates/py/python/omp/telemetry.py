"""Frozen telemetry subscriptions, event views, and extension instruments."""

from __future__ import annotations

import sys
from collections.abc import Callable, Hashable, Iterator, Mapping, Sequence
from contextvars import ContextVar
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from enum import Enum, StrEnum
from types import MappingProxyType, ModuleType
from typing import Any

from _omp import Duration, EnvPath

from ._errors import NotWiredError
from ._registry import ExportDefinition, registry as _declarations
from .placement import Place

QUEUE_DEFAULT = 4096
BATCH_MAX = 1024
METRIC_PREFIX = "omp.ext."


class SubscriptionError(ValueError):
    """A telemetry declaration is malformed or duplicates a static key."""


class Kind(StrEnum):
    """Core-side telemetry event vocabulary."""

    SESSION_START = "session_start"
    SESSION_END = "session_end"
    TURN_START = "turn_start"
    TURN_END = "turn_end"
    MODEL_REQUEST = "model_request"
    MODEL_ATTEMPT = "model_attempt"
    PROVIDER_ERROR = "provider_error"
    TOOL_CALL = "tool_call"
    CAPABILITY_DEGRADED = "capability_degraded"
    COMPACTION = "compaction"
    BRANCH = "branch"
    ARTIFACT_SPILL = "artifact_spill"
    ISSUE_REPORT = "issue_report"
    HOST_WARNING = "host_warning"


class StopReason(StrEnum):
    """Normalized reason a model response stopped."""

    END_TURN = "end_turn"
    TOOL_USE = "tool_use"
    MAX_TOKENS = "max_tokens"
    CONTENT_FILTER = "content_filter"
    UNSPECIFIED = "unspecified"


class Scope(StrEnum):
    """Agent extent visible to a telemetry subscription."""

    SELF = "self"
    TREE = "tree"
    PROJECT = "project"


class Overflow(StrEnum):
    """Bounded-ring behavior when a telemetry sink falls behind."""

    DROP_OLDEST = "drop_oldest"
    DROP_NEWEST = "drop_newest"
    COALESCE_BY_KEY = "coalesce_by_key"


@dataclass(frozen=True, slots=True)
class Tokens:
    """Unabridged token-usage buckets from a settled model request."""

    input: int = 0
    output: int = 0
    cache_read: int = 0
    cache_write: int = 0
    reasoning: int = 0
    total: int = 0
    context: int | None = None
    premium_requests: int = 0
    cache_ttl_5m: int = 0
    cache_ttl_1h: int = 0
    server_web_search: int = 0
    server_web_fetch: int = 0
    orchestration_input: int = 0
    orchestration_output: int = 0
    orchestration_cache_read: int = 0
    detail: Mapping[str, int | float | str] = field(default_factory=dict)

    @property
    def uncached_input(self) -> int:
        """Input tokens not read from or written to a provider cache."""

        return max(0, self.input - self.cache_read - self.cache_write)

    @property
    def cache_hit_rate(self) -> float:
        """Fraction of input tokens served from cache."""

        return self.cache_read / self.input if self.input else 0.0


@dataclass(frozen=True, slots=True)
class PromptFingerprint:
    """Assembler-owned prompt-prefix and cache-breakpoint facts."""

    digest: str
    slots: Mapping[str, str]
    changed: tuple[str, ...]
    prefix_stable_bytes: int
    cache_key: str
    retention: str
    mode: str
    ttl: str
    breakpoint: str
    breakpoint_indices: tuple[int, ...]


@dataclass(frozen=True, slots=True)
class ModelRequest:
    """Frozen subset of a settled model request used by telemetry consumers."""

    seq: int
    usage: Tokens
    prompt: PromptFingerprint
    served_model: str


@dataclass(frozen=True, slots=True)
class DropStats:
    """Loss and delivery counters for one host-side subscription ring."""

    delivered: int
    dropped: int
    coalesced: int
    errored: int
    replay_skipped: int
    queue_depth: int
    first_drop_seq: int | None
    since_ms: int


def _subscribe(
    kinds: Sequence[Kind | str],
    *,
    scope: Scope = Scope.TREE,
    queue: int = QUEUE_DEFAULT,
    overflow: Overflow = Overflow.DROP_OLDEST,
    coalesce_key: Callable[[object], Hashable] | None = None,
    batch: int | None = None,
    replay: bool = False,
    replay_limit: int = 2048,
):
    """Declare a lossy telemetry sink without opening the CONTROL channel."""

    try:
        parsed_kinds = tuple(Kind(kind) for kind in kinds)
        parsed_scope = Scope(scope)
        parsed_overflow = Overflow(overflow)
    except ValueError as error:
        raise SubscriptionError(str(error)) from error
    if not parsed_kinds:
        raise SubscriptionError("telemetry kinds must not be empty")
    if not 1 <= queue <= 65536:
        raise SubscriptionError("telemetry queue must be in 1..=65536")
    if batch is not None and not 2 <= batch <= BATCH_MAX:
        raise SubscriptionError(f"telemetry batch must be in 2..={BATCH_MAX}")
    if replay_limit < 1:
        raise SubscriptionError("telemetry replay_limit must be positive")
    if (parsed_overflow is Overflow.COALESCE_BY_KEY) != (coalesce_key is not None):
        raise SubscriptionError("coalesce_key is required only for coalesce_by_key overflow")

    def decorate(function: Any) -> Any:
        if not callable(function):
            raise TypeError("@omp.telemetry may decorate only a callable")
        _declarations.register_telemetry(
            parsed_kinds, parsed_scope, queue, parsed_overflow, batch, replay, replay_limit, function
        )
        return function

    return decorate


_instrument_sink: ContextVar[Any | None] = ContextVar(
    "omp_telemetry_instrument_sink", default=None
)
_instruments: dict[str, Counter | Histogram] = {}


def _install_instrument_sink(sink: Any | None) -> None:
    """Install the host-owned synchronous instrument sink for this context."""

    _instrument_sink.set(sink)


def _instrument_name(name: str) -> str:
    if not name or name.startswith(("omp.", "gen_ai.", "openai.")):
        raise SubscriptionError("instrument names must be nonempty and outside reserved namespaces")
    return name


def _validate_attrs(attrs: Mapping[str, object]) -> None:
    for value in attrs.values():
        if not isinstance(value, (str, int, float, bool)):
            raise TypeError("instrument attribute values must be str, int, float, or bool")


class Counter:
    """Extension-owned monotonic counter declaration."""

    __slots__ = ("_local", "description", "unit")

    def __init__(self, local: str, unit: str, description: str) -> None:
        self._local = local
        self.unit = unit
        self.description = description

    @property
    def name(self) -> str:
        """Return the fully qualified, reserved-prefix-safe metric name."""

        extension = _declarations.extension_id or "unregistered"
        return f"{METRIC_PREFIX}{extension}.{self._local}"

    def add(self, value: int | float = 1, /, **attrs: str | int | float | bool) -> None:
        """Increment the counter, discarding the value when no exporter is installed."""

        if value < 0:
            raise ValueError("counter increments must be non-negative")
        _validate_attrs(attrs)
        sink = _instrument_sink.get()
        if sink is None:
            return
        sink.add(self.name, value, attrs)


class Histogram:
    """Extension-owned histogram declaration."""

    __slots__ = ("_local", "boundaries", "description", "unit")

    def __init__(
        self,
        local: str,
        unit: str,
        description: str,
        boundaries: tuple[int | float, ...] | None,
    ) -> None:
        self._local = local
        self.unit = unit
        self.description = description
        self.boundaries = boundaries

    @property
    def name(self) -> str:
        """Return the fully qualified, reserved-prefix-safe metric name."""

        extension = _declarations.extension_id or "unregistered"
        return f"{METRIC_PREFIX}{extension}.{self._local}"

    def record(self, value: int | float, /, **attrs: str | int | float | bool) -> None:
        """Record an observation, discarding it when no exporter is installed."""

        _validate_attrs(attrs)
        sink = _instrument_sink.get()
        if sink is None:
            return
        sink.record(self.name, value, attrs)


def counter(name: str, *, unit: str, description: str) -> Counter:
    """Create or return an extension-owned monotonic counter."""

    local = _instrument_name(name)
    existing = _instruments.get(local)
    if existing is not None:
        if (
            not isinstance(existing, Counter)
            or existing.unit != unit
            or existing.description != description
        ):
            raise SubscriptionError(f"conflicting instrument declaration: {local!r}")
        return existing
    instrument = Counter(local, unit, description)
    _instruments[local] = instrument
    return instrument


def histogram(
    name: str,
    *,
    unit: str,
    description: str,
    boundaries: Sequence[int | float] | None = None,
) -> Histogram:
    """Create or return an extension-owned histogram."""

    local = _instrument_name(name)
    parsed_boundaries = tuple(boundaries) if boundaries is not None else None
    if parsed_boundaries is not None and any(
        a >= b for a, b in zip(parsed_boundaries, parsed_boundaries[1:])
    ):
        raise ValueError("histogram boundaries must be strictly increasing")
    existing = _instruments.get(local)
    if existing is not None:
        if (
            not isinstance(existing, Histogram)
            or existing.unit != unit
            or existing.description != description
            or existing.boundaries != parsed_boundaries
        ):
            raise SubscriptionError(f"conflicting instrument declaration: {local!r}")
        return existing
    instrument = Histogram(local, unit, description, parsed_boundaries)
    _instruments[local] = instrument
    return instrument


class ExportError(SubscriptionError):
    """An export target declaration is malformed."""


@dataclass(frozen=True, slots=True)
class ExportTarget:
    """Base class for declarative telemetry export targets."""


_EMPTY_MAP: Mapping[str, Any] = MappingProxyType({})


@dataclass(frozen=True, slots=True)
class OtlpTarget(ExportTarget):
    """An OpenTelemetry Protocol export target."""

    endpoint: str
    protocol: str = "http/protobuf"
    headers: Mapping[str, str] = field(default_factory=lambda: _EMPTY_MAP)
    signals: Sequence[str] = ("traces", "metrics", "logs")
    resource_attributes: Mapping[str, str] = field(default_factory=lambda: _EMPTY_MAP)
    timeout: Duration = Duration("10s")
    compression: str | None = "gzip"


@dataclass(frozen=True, slots=True)
class ProcessTarget(ExportTarget):
    """An Environment-supervised process export target."""

    process: str
    framing: str = "jsonl"
    flush_every: Duration = Duration("1s")
    handshake: Mapping[str, object] | None = None


@dataclass(frozen=True, slots=True)
class FileTarget(ExportTarget):
    """An Environment-file export target."""

    path: EnvPath
    framing: str = "jsonl"
    rotate_bytes: int = 64 * 1024 * 1024
    keep: int = 4


@dataclass(frozen=True, slots=True)
class ExportStats:
    """Delivery statistics for one registered export target."""

    sent: int = 0
    dropped: int = 0
    failures: int = 0
    queue_depth: int = 0
    last_flush_ms: int = 0
    last_error: str | None = None
    backoff_ms: int = 0


class ExportHandle:
    """Live handle for a declaratively registered export target."""

    __slots__ = ("_target",)

    def __init__(self, target: ExportTarget) -> None:
        self._target = target

    @property
    def target(self) -> ExportTarget:
        """Return the registered target."""

        return self._target

    async def stop(self) -> None:
        """Stop this export target after a final flush."""

        raise NotWiredError("omp.telemetry.ExportHandle.stop")

    async def stats(self) -> ExportStats:
        """Return current delivery statistics for this export target."""

        raise NotWiredError("omp.telemetry.ExportHandle.stats")


def export(
    target: ExportTarget,
    *,
    kinds: Sequence[Kind | str] = (),
    sample: float = 1.0,
) -> ExportHandle:
    """Register a host-owned telemetry export target."""

    if not isinstance(target, ExportTarget):
        raise ExportError("target must be an ExportTarget")
    try:
        parsed_kinds = tuple(Kind(kind) for kind in kinds)
    except ValueError as error:
        raise ExportError(str(error)) from error
    if not 0.0 <= sample <= 1.0:
        raise ExportError("sample must be in 0.0..=1.0")
    if isinstance(target, OtlpTarget) and target.protocol != "http/protobuf":
        raise ExportError("unsupported OTLP protocol")
    if isinstance(target, (ProcessTarget, FileTarget)) and target.framing not in {
        "jsonl",
        "lenprefix",
    }:
        raise ExportError("unsupported export framing")
    definition = ExportDefinition(
        target=target,
        kinds=tuple(kind.value for kind in parsed_kinds),
        sample=sample,
    )
    _declarations.register_export(definition)
    return ExportHandle(target)


async def flush(*, timeout: Duration = Duration("10s")) -> bool:
    """Force every registered export target to flush."""

    del timeout
    raise NotWiredError("omp.telemetry.flush")


def dropped(sink: object | None = None) -> DropStats | Mapping[str, DropStats]:
    """Read host-side loss counters for one or all subscriptions."""

    del sink
    raise NotWiredError("omp.telemetry.dropped")


@dataclass(frozen=True, slots=True)
class Cost:
    """Represent settled telemetry cost in exact nano-USD."""

    nanos_usd: int
    estimated: bool
    input_nanos_usd: int | None
    output_nanos_usd: int | None
    cache_read_nanos_usd: int | None
    cache_write_nanos_usd: int | None
    unavailable_reason: str | None

    @property
    def usd(self) -> float:
        """Return the total cost in USD for display."""

        return self.nanos_usd / 1_000_000_000


@dataclass(frozen=True, slots=True)
class ContextSnapshot:
    """Capture context-window occupancy at a telemetry boundary."""

    prompt_tokens: int
    non_message_tokens: int
    history_rewrite_tokens_removed: int
    last_message_at_ms: int | None
    window: int
    percent: float


@dataclass(frozen=True, slots=True)
class TraceRef:
    """Identify the OpenTelemetry span under which an event was emitted."""

    trace_id: str
    span_id: str
    sampled: bool


@dataclass(frozen=True, slots=True)
class ExtensionRef:
    """Attribute a telemetry record to one exact installed extension build."""

    publisher: str
    id: str
    version: str
    digest: str
    layer: str
    trust: str
    generation: int


@dataclass(frozen=True, slots=True)
class Envelope:
    """Carry the common identity, ordering, and trace prefix of every event."""

    kind: Kind
    seq: int
    at_ms: int
    session: str
    agent: str
    depth: int
    conversation: str
    trace: TraceRef | None
    principal: str
    generation: int


@dataclass(frozen=True, slots=True)
class SessionStart(Envelope):
    """Describe a session opening or resuming."""

    resumed: bool
    parent: str | None
    cwd: EnvPath
    place: Place
    remote: str | None
    model: str
    provider: str
    devices: tuple[str, ...]
    core_tools: tuple[str, ...]
    extensions: tuple[ExtensionRef, ...]
    schema_rev: str
    prompt: PromptFingerprint
    registry_hash: str


@dataclass(frozen=True, slots=True)
class SessionEnd(Envelope):
    """Describe final lifetime totals for a settled session."""

    reason: str
    turns: int
    requests: int
    calls: int
    tokens: Tokens
    cost: Cost | None
    wall_ms: int
    faults: int
    issues: int


@dataclass(frozen=True, slots=True)
class TurnStart(Envelope):
    """Describe the input shape and route selected for a new turn."""

    turn: int
    trigger: str
    input_chars: int
    input_parts: int
    attachments: int
    model: str
    effort: str | None


@dataclass(frozen=True, slots=True)
class TurnEnd(Envelope):
    """Describe the settled usage, latency, and outcome of one turn."""

    turn: int
    steps: int
    requests: int
    calls: int
    tokens: Tokens
    cost: Cost | None
    latency_ms: int
    stop: StopReason
    tools_used: tuple[str, ...]
    faults: int
    interrupted: bool
    context: ContextSnapshot


@dataclass(frozen=True, slots=True)
class Predicate:
    """Base value for a host-evaluated telemetry query predicate."""


@dataclass(frozen=True, slots=True)
class Eq(Predicate):
    """Require a telemetry field to equal ``value``."""

    value: object


@dataclass(frozen=True, slots=True)
class Step:
    """Describe one element of an ordered telemetry match sequence."""

    kinds: Sequence[Kind] = ()
    tool: str | None = None
    target: str | None = None
    rev: str | None = None
    where: Mapping[str, Predicate] = field(default_factory=dict)
    name: str | None = None


@dataclass(frozen=True, slots=True)
class Query:
    """Describe a host-side query over the durable telemetry index."""

    match: Sequence[Step]
    window: int = 8
    same_turn: bool = True
    scope: Scope = Scope.PROJECT
    sessions: Sequence[str] = ()
    since: datetime | timedelta | None = None
    until: datetime | None = None
    select: Sequence[str] = ()
    group_by: Sequence[str] = ()
    order_by: Sequence[str] = ()
    limit: int = 1000
    cursor: str | None = None


@dataclass(frozen=True, slots=True)
class Row(Mapping[str, object]):
    """Expose projected fields and the events matched by one query row."""

    events: tuple[Envelope, ...]
    bindings: Mapping[str, Envelope]
    session: str
    turn: int
    _values: Mapping[str, object] = field(default_factory=dict, repr=False)

    def __getitem__(self, key: str) -> object:
        """Return one projected field or aggregate."""

        return self._values[key]

    def __iter__(self) -> Iterator[str]:
        """Iterate projected field or aggregate names."""

        return iter(self._values)

    def __len__(self) -> int:
        """Return the number of projected fields or aggregates."""

        return len(self._values)


@dataclass(frozen=True, slots=True)
class QueryResult:
    """Report rows and scan facts from a settled telemetry query."""

    rows: tuple[Row, ...]
    total: int
    cursor: str | None
    truncated: bool
    scanned_sessions: int
    scanned_events: int
    backfilled: bool
    floored: bool
    elapsed_ms: int


def _query_wire(value: object) -> object:
    if isinstance(value, Eq):
        return {"op": "eq", "value": _query_wire(value.value)}
    if isinstance(value, Step):
        return {
            "kinds": [_query_wire(kind) for kind in value.kinds],
            "tool": value.tool,
            "target": value.target,
            "rev": value.rev,
            "where": {path: _query_wire(predicate) for path, predicate in value.where.items()},
            "name": value.name,
        }
    if isinstance(value, Query):
        return {
            "match": [_query_wire(step) for step in value.match],
            "window": value.window,
            "same_turn": value.same_turn,
            "scope": value.scope.value,
            "sessions": list(value.sessions),
            "since": _query_wire(value.since),
            "until": _query_wire(value.until),
            "select": list(value.select),
            "group_by": list(value.group_by),
            "order_by": list(value.order_by),
            "limit": value.limit,
            "cursor": value.cursor,
        }
    if isinstance(value, datetime):
        return value.isoformat()
    if isinstance(value, timedelta):
        return value.total_seconds()
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, Mapping):
        return {str(key): _query_wire(item) for key, item in value.items()}
    if isinstance(value, Sequence) and not isinstance(value, (str, bytes, bytearray)):
        return [_query_wire(item) for item in value]
    return value


async def query(q: Query) -> QueryResult:
    """Run a serialized query through the host CONTROL bridge."""

    from . import _control_backend, _control_request

    if _control_backend.get() is None:
        raise NotWiredError("omp.telemetry.query")
    return await _control_request("omp.telemetry.query", query=_query_wire(q))


class _TelemetryModule(ModuleType):
    def __call__(self, kinds: Sequence[Kind | str], **kwargs: object):
        return _subscribe(kinds, **kwargs)


sys.modules[__name__].__class__ = _TelemetryModule

__all__ = (
    "BATCH_MAX", "ContextSnapshot", "Cost", "Counter", "DropStats", "Envelope", "Eq",
    "ExportError", "ExportHandle", "ExportStats", "ExportTarget", "ExtensionRef", "FileTarget",
    "Histogram", "Kind", "METRIC_PREFIX", "ModelRequest", "OtlpTarget", "Overflow", "Predicate",
    "ProcessTarget", "PromptFingerprint", "Query", "QueryResult", "QUEUE_DEFAULT", "Row", "Scope",
    "SessionEnd", "SessionStart", "Step", "StopReason", "SubscriptionError", "Tokens", "TraceRef",
    "TurnEnd", "TurnStart", "counter", "dropped", "export", "flush", "histogram", "query",
)
