"""Frozen telemetry subscriptions, event views, and extension instruments."""

from __future__ import annotations

import sys
from collections.abc import Callable, Hashable, Mapping, Sequence
from dataclasses import dataclass, field
from enum import StrEnum
from types import ModuleType
from typing import Any

from ._errors import NotWiredError
from ._registry import registry as _declarations

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


class Counter:
    """Extension-owned monotonic counter declaration."""

    __slots__ = ("_name",)

    def __init__(self, name: str) -> None:
        self._name = _instrument_name(name)

    @property
    def name(self) -> str:
        """Fully qualified, reserved-prefix-safe metric name."""

        return self._name

    def add(self, value: int | float = 1, /, **attrs: str | int | float | bool) -> None:
        """Add to this counter through the future telemetry host arm."""

        del value, attrs
        raise NotWiredError("omp.telemetry.Counter.add")


class Histogram:
    """Extension-owned histogram declaration."""

    __slots__ = ("_name",)

    def __init__(self, name: str) -> None:
        self._name = _instrument_name(name)

    @property
    def name(self) -> str:
        """Fully qualified, reserved-prefix-safe metric name."""

        return self._name

    def record(self, value: int | float, /, **attrs: str | int | float | bool) -> None:
        """Record an observation through the future telemetry host arm."""

        del value, attrs
        raise NotWiredError("omp.telemetry.Histogram.record")


def _instrument_name(name: str) -> str:
    if not name or name.startswith(("omp.", "gen_ai.", "openai.")):
        raise SubscriptionError("instrument names must be nonempty and outside reserved namespaces")
    return f"{METRIC_PREFIX}<extension>.{name}"


def counter(name: str, *, unit: str, description: str) -> Counter:
    """Declare an extension-owned monotonic counter."""

    del unit, description
    return Counter(name)


def histogram(
    name: str,
    *,
    unit: str,
    description: str,
    boundaries: Sequence[int | float] | None = None,
) -> Histogram:
    """Declare an extension-owned histogram."""

    del unit, description
    if boundaries is not None and any(a >= b for a, b in zip(boundaries, boundaries[1:])):
        raise ValueError("histogram boundaries must be strictly increasing")
    return Histogram(name)


def dropped(sink: object | None = None) -> DropStats | Mapping[str, DropStats]:
    """Read host-side loss counters for one or all subscriptions."""

    del sink
    raise NotWiredError("omp.telemetry.dropped")


class _TelemetryModule(ModuleType):
    def __call__(self, kinds: Sequence[Kind | str], **kwargs: object):
        return _subscribe(kinds, **kwargs)


sys.modules[__name__].__class__ = _TelemetryModule

__all__ = (
    "BATCH_MAX", "Counter", "DropStats", "Histogram", "Kind", "METRIC_PREFIX",
    "ModelRequest", "Overflow", "PromptFingerprint", "QUEUE_DEFAULT", "Scope",
    "SubscriptionError", "Tokens", "counter", "dropped", "histogram",
)
