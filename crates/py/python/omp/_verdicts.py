"""Pure, frozen verdict and projection vocabulary."""

from __future__ import annotations

import base64
import dataclasses
import json
from collections.abc import AsyncIterator
from dataclasses import dataclass
from enum import Enum, IntEnum, StrEnum
from types import MappingProxyType
from typing import Any, Generic, Mapping, TypeVar

from _omp import ArtifactUrl, Duration, InvocationPhase

from ._context import Context
from ._errors import NotWiredError


class Payload:
    """Marker base for a device's durable success value."""

    __slots__ = ()

    def __new__(cls, *_args: Any, **_kwargs: Any) -> Payload:
        if cls is Payload:
            raise TypeError("Payload is a marker base; instantiate a frozen dataclass subclass")
        return super().__new__(cls)

    def useless(self) -> bool:
        """Return whether compaction may omit this value's prompt projection."""
        return False


_P = TypeVar("_P", bound=Payload)
_F = TypeVar("_F")
_U = TypeVar("_U")
_R = TypeVar("_R")
_UPDATE_MISSING = object()


@dataclass(frozen=True, slots=True, init=False)
class Update(Generic[_U]):
    """An ephemeral typed progress payload emitted by a streaming device."""

    payload: _U

    def __init__(
        self, payload: _U | object = _UPDATE_MISSING, /, **fields: object
    ) -> None:
        if payload is not _UPDATE_MISSING and fields:
            raise TypeError("Update accepts either one payload or keyword fields")
        value = fields if payload is _UPDATE_MISSING else payload
        object.__setattr__(self, "payload", value)


@dataclass(frozen=True, slots=True)
class Done(Generic[_R]):
    """The terminal result emitted by a streaming device."""

    result: _R
    useless: bool = False



@dataclass(frozen=True, slots=True)
class JobRef:
    """Name detached Environment-owned work and its expected artifact."""

    id: str
    owner_kind: str
    owner_name: str
    owner_generation: int
    description: str
    media_type: str | None
    lifetime: str


@dataclass(frozen=True, slots=True)
class Detached:
    """Terminate this turn while supervised work continues on the job board."""

    job: JobRef


class _Jobs:
    """Host-backed detached-job registration operations."""

    __slots__ = ()

    async def register(
        self, frames: AsyncIterator[Update[Any] | Done[Any]], ctx: Context
    ) -> JobRef:
        """Register an env-placed device stream as supervised detached work."""

        from . import _control_backend, _control_request

        operation = "omp.jobs.register"
        if _control_backend.get() is None:
            raise NotWiredError(operation)
        return await _control_request(operation, frames=frames, context=ctx)


jobs = _Jobs()
"""Host-backed detached-job registration namespace."""





@dataclass(frozen=True, slots=True)
class Ok(Generic[_P]):
    """A settled successful call and its durable payload."""

    payload: _P


@dataclass(frozen=True, slots=True)
class Faulted(Generic[_F]):
    """A settled expected failure and its durable fault value."""

    fault: _F


class AbortKind(StrEnum):
    """Classify why a call settled without a normal device verdict."""

    CANCELLED = "cancelled"
    SKIPPED = "skipped"
    POLICY_DENIED = "policy_denied"


@dataclass(frozen=True, slots=True)
class ArgsRejected:
    """Record a harness-owned structured argument rejection."""

    issue: object


@dataclass(frozen=True, slots=True)
class Aborted:
    """Record a harness- or Core-owned abnormal call settlement."""

    abort: object
    kind: AbortKind
    policy: object | None = None

    def __post_init__(self) -> None:
        """Enforce that only policy denials carry a structured policy value."""
        has_policy = self.policy is not None
        if has_policy != (self.kind is AbortKind.POLICY_DENIED):
            raise ValueError(
                "policy must be present exactly when kind is AbortKind.POLICY_DENIED"
            )


CallOutcome = Ok[_P] | Faulted[_F] | ArgsRejected | Aborted
"""Closed union of the four durable call-outcome arms."""


@dataclass(frozen=True, slots=True)
class ArtifactRef:
    """Reference durable bytes in the session artifact namespace."""

    id: str
    hash: str
    media_type: str
    byte_len: int

    @property
    def url(self) -> ArtifactUrl:
        """Return this reference's typed ``artifact://`` address."""

        return ArtifactUrl(f"artifact://{self.id}")


class Dialect(StrEnum):
    """Argument dialect used by a model-facing projection."""

    HASHLINE = "hl"
    REPLACE = "rep"
    PATCH = "patch"
    NATIVE = "native"


class ModelClass(IntEnum):
    """Coarse model capability band used only to size projections."""

    TINY = 0
    SMALL = 1
    STANDARD = 2
    FRONTIER = 3


@dataclass(frozen=True, slots=True)
class PromptCaps:
    """Deterministic limits for one model-facing projection."""

    maximum_parts: int
    maximum_text_bytes: int
    media: bool
    dialect: Dialect
    model_class: ModelClass

    def __post_init__(self) -> None:
        if (
            isinstance(self.maximum_parts, bool)
            or not isinstance(self.maximum_parts, int)
            or self.maximum_parts < 0
        ):
            raise ValueError("maximum_parts must be a non-negative integer")
        if (
            isinstance(self.maximum_text_bytes, bool)
            or not isinstance(self.maximum_text_bytes, int)
            or self.maximum_text_bytes < 0
        ):
            raise ValueError("maximum_text_bytes must be a non-negative integer")

    def fits(self, text: str) -> bool:
        """Return whether one text part fits this projection budget."""
        return self.maximum_parts > 0 and len(text.encode("utf-8")) <= self.maximum_text_bytes


@dataclass(frozen=True, slots=True)
class TextPart:
    """UTF-8 text exposed to the model."""

    text: str


@dataclass(frozen=True, slots=True)
class JsonPart:
    """Canonical JSON bytes exposed as structured model content."""

    json: bytes


@dataclass(frozen=True, slots=True)
class BlobPart:
    """A blob-backed media part with deterministic fallback text."""

    blob: Any
    alt: str | None = None


class Part:
    """Validated factory for model-facing projection parts."""

    __slots__ = ()

    @staticmethod
    def text(text: str) -> TextPart:
        """Construct a text part."""
        if not isinstance(text, str):
            raise TypeError("text part content must be str")
        return TextPart(text)

    @staticmethod
    def json(value: object) -> JsonPart:
        """Construct a canonical JSON part."""
        return JsonPart(_canonical_json(value))

    @staticmethod
    def blob(ref: Any, alt: str | None = None) -> BlobPart:
        """Construct a blob-backed media part."""
        if alt is not None and not isinstance(alt, str):
            raise TypeError("blob alt text must be str or None")
        return BlobPart(ref, alt)


class Budget:
    """Whole-fragment accumulator enforcing a ``PromptCaps`` budget."""

    __slots__ = ("_caps", "_parts", "_text_bytes", "_truncated", "_sealed")

    def __init__(self, caps: PromptCaps) -> None:
        if not isinstance(caps, PromptCaps):
            raise TypeError("caps must be PromptCaps")
        self._caps = caps
        self._parts: list[TextPart | JsonPart | BlobPart] = []
        self._text_bytes = 0
        self._truncated = False
        self._sealed = False

    @property
    def remaining(self) -> int:
        """Return the unconsumed UTF-8 byte budget."""
        return max(0, self._caps.maximum_text_bytes - self._text_bytes)

    def push(self, fragment: str) -> bool:
        """Append a whole text fragment when it fits."""
        self._ensure_open()
        if not isinstance(fragment, str):
            raise TypeError("projection fragments must be str")
        size = len(fragment.encode("utf-8"))
        needs_part = not self._parts or not isinstance(self._parts[-1], TextPart)
        if size > self.remaining or (
            needs_part and len(self._parts) >= self._caps.maximum_parts
        ):
            self._truncated = True
            return False
        if needs_part:
            self._parts.append(TextPart(fragment))
        else:
            previous = self._parts[-1]
            assert isinstance(previous, TextPart)
            self._parts[-1] = TextPart(previous.text + fragment)
        self._text_bytes += size
        return True

    def push_json(self, value: object) -> bool:
        """Append one canonical JSON part when it fits."""
        self._ensure_open()
        raw = _canonical_json(value)
        if len(raw) > self.remaining or len(self._parts) >= self._caps.maximum_parts:
            self._truncated = True
            return False
        self._parts.append(JsonPart(raw))
        self._text_bytes += len(raw)
        return True

    def push_blob(self, ref: Any, alt: str) -> bool:
        """Append media, or its fallback text when media is unavailable."""
        self._ensure_open()
        if not self._caps.media:
            return self.push(alt)
        if len(self._parts) >= self._caps.maximum_parts:
            self._truncated = True
            return False
        self._parts.append(BlobPart(ref, alt))
        return True

    def finish(self) -> list[TextPart | JsonPart | BlobPart]:
        """Seal and return the accepted parts, marking truncation when possible."""
        self._ensure_open()
        marker = "\n[truncated]"
        if self._truncated:
            marker_size = len(marker.encode("utf-8"))
            can_merge = bool(self._parts) and isinstance(self._parts[-1], TextPart)
            if marker_size <= self.remaining and (
                can_merge or len(self._parts) < self._caps.maximum_parts
            ):
                if can_merge:
                    previous = self._parts[-1]
                    assert isinstance(previous, TextPart)
                    self._parts[-1] = TextPart(previous.text + marker)
                else:
                    self._parts.append(TextPart(marker))
                self._text_bytes += marker_size
        self._sealed = True
        return list(self._parts)

    def _ensure_open(self) -> None:
        if self._sealed:
            raise RuntimeError("projection budget is sealed")


@dataclass(frozen=True, slots=True, order=True)
class Rev:
    """One argument-and-projection dialect revision."""

    family: str
    n: int

    def __post_init__(self) -> None:
        if not self.family or "." in self.family:
            if self.family != "":
                raise ValueError("revision family must be empty or a non-empty dotless name")
        if not isinstance(self.n, int) or isinstance(self.n, bool) or not 0 <= self.n <= 65535:
            raise ValueError("revision number must be a u16 integer")

    def __str__(self) -> str:
        return f"{self.family}.{self.n}" if self.family else str(self.n)

    @classmethod
    def parse(cls, value: str) -> Rev:
        """Parse ``family.n`` or a bare numeric revision."""
        if not isinstance(value, str) or not value:
            raise ValueError("revision must be a non-empty string")
        family, separator, number = value.rpartition(".")
        if not separator:
            family, number = "", value
        if not number.isascii() or not number.isdigit():
            raise ValueError(f"malformed revision: {value!r}")
        return cls(family, int(number))


@dataclass(frozen=True, slots=True, order=True)
class ToolIdentity:
    """Durable device name and semantic revision."""

    name: str
    rev: Rev

    def __str__(self) -> str:
        return f"{self.name}@{self.rev}"


_EMPTY_PRESENTATION: Mapping[str, object] = MappingProxyType({})


@dataclass(frozen=True, slots=True)
class View(Generic[_U, _P, _F]):
    """Immutable live-or-settled renderer fold input."""

    identity: ToolIdentity
    call_id: str
    updates: tuple[_U, ...]
    state: object | None
    verdict: CallOutcome[_P, _F] | None
    elapsed: Duration
    phase: InvocationPhase
    presentation: Mapping[str, object] = dataclasses.field(
        default_factory=lambda: _EMPTY_PRESENTATION
    )

    def __post_init__(self) -> None:
        """Freeze the host-materialized presentation snapshot."""

        if self.presentation is not _EMPTY_PRESENTATION:
            object.__setattr__(
                self,
                "presentation",
                MappingProxyType(dict(self.presentation)),
            )


@dataclass(frozen=True, slots=True)
class RecordedCall:
    """Byte-exact historical call supplied to a lift step."""

    identity: ToolIdentity
    raw_args: bytes
    verdict: bytes


@dataclass(frozen=True, slots=True)
class LiftedCall:
    """A historical call re-expressed at a destination revision."""

    raw_args: bytes
    verdict: bytes

    @classmethod
    def of(cls, args: object, verdict: object) -> LiftedCall:
        """Canonically serialize lifted arguments and verdict."""
        return cls(_canonical_json(args), _canonical_json(verdict))


class ArtifactLifetime(StrEnum):
    """Minimum retention requested for a spilled verdict artifact."""

    EPHEMERAL = "ephemeral"
    SESSION = "session"
    DURABLE = "durable"


SPILL_INLINE_LIMIT = 16 * 1024
"""Default maximum canonical verdict size retained inline."""


@dataclass(frozen=True, slots=True)
class SpillBudget:
    """Policy controlling central artifactization of a large verdict."""

    inline_limit: int = SPILL_INLINE_LIMIT
    media_type: str = "application/json"
    lifetime: ArtifactLifetime = ArtifactLifetime.SESSION
    always: bool = False


def prompt(view: Ok[Any] | Faulted[Any], caps: PromptCaps) -> list[TextPart | JsonPart | BlobPart]:
    """Dispatch a prompt projection once the host projection arm is wired."""
    raise NotWiredError("verdict prompt projection dispatch is not wired")


def _canonical_json(value: object) -> bytes:
    def default(item: object) -> object:
        if dataclasses.is_dataclass(item) and not isinstance(item, type):
            return {field.name: getattr(item, field.name) for field in dataclasses.fields(item)}
        if isinstance(item, Enum):
            return item.value
        if isinstance(item, bytes):
            return {"$bytes": base64.b64encode(item).decode("ascii")}
        raise TypeError(f"{type(item).__name__} is not verdict-serializable")

    return json.dumps(value, default=default, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
