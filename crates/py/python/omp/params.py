"""Typed, linear parameter cursors and argument-finalization vocabulary.

The module is import-time inert.  Cursor operations are routed through the active
host CONTROL backend; declarations, validation, cursor ownership, and interrupt
bookkeeping are enforced locally before a request crosses that boundary.
"""

from __future__ import annotations

import asyncio
import contextlib
import dataclasses
from collections import deque
from collections.abc import AsyncIterator, Callable, Coroutine, Mapping
from dataclasses import dataclass
from enum import StrEnum
from types import MappingProxyType
from typing import (
    Annotated,
    Any,
    ClassVar,
    TypeAlias,
    get_args,
    get_origin,
    get_type_hints,
)

from _omp import Duration, InvocationPhase, OmpError

from ._verdicts import Aborted, Detached, Done, Rev, Update


class ArgIssueKind(StrEnum):
    """Classify one stable argument-finalization failure."""

    MISSING = "missing"
    INCOMPLETE = "incomplete"
    ABORTED = "aborted"
    MALFORMED = "malformed"
    TYPE_MISMATCH = "type_mismatch"
    AMBIGUOUS = "ambiguous"
    PROTOCOL = "protocol"


@dataclass(frozen=True, slots=True)
class ArgIssue:
    """Describe a path-addressed argument failure without rendering policy."""

    path: tuple[str | int, ...]
    expected: str
    kind: ArgIssueKind
    example: str | None = None
    found: str | None = None

    def __post_init__(self) -> None:
        path = tuple(self.path)
        if any(
            not isinstance(part, (str, int))
            or isinstance(part, bool)
            or isinstance(part, int) and part < 0
            for part in path
        ):
            raise TypeError("argument issue path parts must be strings or integers")
        if not isinstance(self.expected, str) or not self.expected:
            raise TypeError("argument issue expected shape must be a non-empty string")
        if not isinstance(self.kind, ArgIssueKind):
            raise TypeError("argument issue kind must be an ArgIssueKind")
        if self.example is not None and not isinstance(self.example, str):
            raise TypeError("argument issue example must be str or None")
        if self.found is not None and not isinstance(self.found, str):
            raise TypeError("argument issue found shape must be str or None")
        object.__setattr__(self, "path", path)


class ArgFault(ValueError, OmpError):
    """Raise one structured argument issue from a pull or finalization."""

    def __init__(
        self,
        issue_or_path: ArgIssue | tuple[str | int, ...],
        kind: ArgIssueKind | None = None,
        detail: str | None = None,
        example: str | None = None,
    ) -> None:
        if isinstance(issue_or_path, ArgIssue):
            if kind is not None or detail is not None or example is not None:
                raise TypeError("ArgFault(issue) accepts no additional payload fields")
            issue = issue_or_path
            detail = issue.expected
            if issue.found is not None:
                detail = f"{detail}; found {issue.found}"
        else:
            if not isinstance(kind, ArgIssueKind):
                raise TypeError("ArgFault path payload requires an ArgIssueKind")
            if not isinstance(detail, str) or not detail:
                raise TypeError("ArgFault path payload requires non-empty detail")
            issue = ArgIssue(tuple(issue_or_path), detail, kind, example=example)
        self.issue = issue
        self.path = issue.path
        self.kind = issue.kind
        self.detail = detail
        self.example = issue.example
        super().__init__(self._message())

    def _message(self) -> str:
        path = "$" + "".join(
            f"[{part}]" if isinstance(part, int) else f".{part}" for part in self.path
        )
        suffix = "" if self.example is None else f"; example: {self.example}"
        return f"argument {path} is {self.kind.value}: {self.detail}{suffix}"


class InvocationEnded(OmpError):
    """Base class for clean invocation termination while a device is running."""


class CommitAborted(InvocationEnded):
    """The assistant item disappeared before effects could be authorized."""

    def __init__(self, detail: str = "assistant item was not committed") -> None:
        self.detail = detail
        super().__init__(detail)


class ParamsMisuse(OmpError):
    """The extension violated linear cursor ownership."""


class ParamsProtocol(OmpError):
    """The host or transport violated invocation framing."""

    def __init__(self, detail: str) -> None:
        if not isinstance(detail, str) or not detail:
            raise TypeError("ParamsProtocol detail must be a non-empty string")
        self.detail = detail
        super().__init__(detail)


@dataclass(frozen=True, slots=True)
class Interrupt:
    """A structured cooperative interrupt supplied by the invocation loop."""

    STEERING: ClassVar[str] = "steering"
    ESCAPE: ClassVar[str] = "escape"
    DEADLINE: ClassVar[str] = "deadline"
    SHUTDOWN: ClassVar[str] = "shutdown"

    kind: str
    reason: str

    def __post_init__(self) -> None:
        if not isinstance(self.kind, str) or not self.kind:
            raise TypeError("interrupt kind must be a non-empty string")
        if not isinstance(self.reason, str):
            raise TypeError("interrupt reason must be a string")


class Interrupted(InvocationEnded):
    """An interruptible parameter operation observed a structured interrupt."""

    def __init__(self, interrupt: Interrupt) -> None:
        if not isinstance(interrupt, Interrupt):
            raise TypeError("Interrupted requires an Interrupt")
        self.interrupt = interrupt
        self.kind = interrupt.kind
        self.reason = interrupt.reason
        super().__init__(f"{interrupt.kind} interrupt: {interrupt.reason}")


class InterruptClosed(InvocationEnded):
    """The invocation owner disappeared before another interrupt arrived."""

    def __init__(self, detail: str = "interrupt stream closed") -> None:
        self.detail = detail
        super().__init__(detail)


@dataclass(frozen=True, slots=True)
class Abort:
    """Structured reason an invocation produced no normal device verdict."""

    SKIPPED: ClassVar[str] = "skipped"
    INTERRUPTED: ClassVar[str] = "interrupted"
    EFFECTS_UNKNOWN: ClassVar[str] = "effects_unknown"
    INPUT_DROPPED: ClassVar[str] = "input_dropped"
    MISSING_OUTCOME: ClassVar[str] = "missing_outcome"

    kind: str
    detail: str | None = None

    def __post_init__(self) -> None:
        if self.kind not in {
            self.SKIPPED,
            self.INTERRUPTED,
            self.EFFECTS_UNKNOWN,
            self.INPUT_DROPPED,
            self.MISSING_OUTCOME,
        }:
            raise ValueError(f"unknown abort kind {self.kind!r}")
        if self.detail is not None and not isinstance(self.detail, str):
            raise TypeError("abort detail must be str or None")

    @classmethod
    def skipped(cls, reason: str) -> Abort:
        """Report a call deliberately not started."""
        return cls(cls.SKIPPED, _reason(reason))

    @classmethod
    def interrupted(cls, reason: str) -> Abort:
        """Report interruption before any effect could land."""
        return cls(cls.INTERRUPTED, _reason(reason))

    @classmethod
    def effects_unknown(cls, reason: str) -> Abort:
        """Report cancellation racing an effect with unknown world state."""
        return cls(cls.EFFECTS_UNKNOWN, _reason(reason))

    @classmethod
    def input_dropped(cls) -> Abort:
        """Report an invocation feed abandoned before item commitment."""
        return cls(cls.INPUT_DROPPED)

    @classmethod
    def missing_outcome(cls) -> Abort:
        """Report a device stream that ended without a terminal event."""
        return cls(cls.MISSING_OUTCOME)


def _reason(value: str) -> str:
    if not isinstance(value, str) or not value:
        raise TypeError("abort reason must be a non-empty string")
    return value


@dataclass(frozen=True, slots=True)
class Args:
    """Terminal event carrying one structured pulled-argument failure."""

    issue: ArgIssue

    def __post_init__(self) -> None:
        if not isinstance(self.issue, ArgIssue):
            raise TypeError("Args requires an ArgIssue")


@dataclass(frozen=True, slots=True, init=False)
class Alias:
    """Declare additional accepted spellings for one canonical parameter key."""

    names: tuple[str, ...]

    def __init__(self, *names: str) -> None:
        if not names:
            raise TypeError("Alias requires at least one name")
        if any(not isinstance(name, str) or not name for name in names):
            raise TypeError("alias names must be non-empty strings")
        if len(set(names)) != len(names):
            raise ValueError("alias names must be unique")
        object.__setattr__(self, "names", tuple(names))


class RepairKind(StrEnum):
    """Classify every charitable surface repair recorded by finalization."""

    ALIAS = "alias"
    COERCION = "coercion"
    TOLERANCE = "tolerance"
    ELISION = "elision"


@dataclass(frozen=True, slots=True)
class Repair:
    """Record one exact, path-addressed argument repair."""

    path: tuple[str | int, ...]
    kind: RepairKind
    detail: str

    def __post_init__(self) -> None:
        path = tuple(self.path)
        if any(
            not isinstance(part, (str, int))
            or isinstance(part, bool)
            or isinstance(part, int) and part < 0
            for part in path
        ):
            raise TypeError("repair path parts must be strings or integers")
        if not isinstance(self.kind, RepairKind):
            raise TypeError("repair kind must be a RepairKind")
        if not isinstance(self.detail, str) or not self.detail:
            raise TypeError("repair detail must be a non-empty string")
        object.__setattr__(self, "path", path)


MAX_NESTING_DEPTH = 128
INTERRUPT_GRACE = Duration("150ms")
MAX_PENDING_PULLS = 1
_PARAM_OPERATIONS = MappingProxyType(
    {
        "args": "omp.params.args",
        "raw": "omp.params.raw",
        "committed": "omp.params.committed",
        "next_interrupt": "omp.params.next_interrupt",
        "pull": "omp.params.pull",
        "array_next": "omp.params.array_next",
        "object_next": "omp.params.object_next",
    }
)


def params(cls: type[Any] | None = None) -> type[Any] | Callable[[type[Any]], type[Any]]:
    """Freeze a parameter dataclass and retain its declarative field metadata."""

    def decorate(target: type[Any]) -> type[Any]:
        if not isinstance(target, type):
            raise TypeError("omp.params decorates a class")
        _lower_alias_metadata(target)
        if dataclasses.is_dataclass(target):
            settings = target.__dataclass_params__
            if not settings.frozen or not hasattr(target, "__slots__"):
                raise TypeError("an existing params dataclass must be frozen and slotted")
            result = target
        else:
            result = dataclass(frozen=True, slots=True)(target)
        setattr(result, "__omp_params__", True)
        setattr(result, "__omp_param_fields__", _compile_param_fields(result))
        return result

    return decorate if cls is None else decorate(cls)


def _compile_param_fields(
    target: type[Any],
) -> Mapping[str, tuple[object, bool, tuple[str, ...], tuple[object, ...], str | None]]:
    """Compile field pull metadata once, never during an invocation."""

    from . import Coerce, Field

    compiled: dict[
        str, tuple[object, bool, tuple[str, ...], tuple[object, ...], str | None]
    ] = {}
    claimed_names: dict[str, str] = {}
    for name, annotation in getattr(target, "__annotations__", {}).items():
        base = annotation
        metadata: tuple[object, ...] = ()
        if get_origin(annotation) is Annotated:
            base, *items = get_args(annotation)
            metadata = tuple(items)
        fields = tuple(item for item in metadata if isinstance(item, Field))
        if len(fields) > 1:
            raise TypeError(f"parameter {name!r} carries more than one omp.Field")
        field = fields[0] if fields else Field()
        for spelling in (name, *field.alias):
            incumbent = claimed_names.get(spelling)
            if incumbent is not None:
                raise ValueError(
                    f"parameter spelling {spelling!r} is shared by "
                    f"{incumbent!r} and {name!r}"
                )
            claimed_names[spelling] = name
        coercions = field.coerce + tuple(
            item for item in metadata if isinstance(item, Coerce)
        )
        compiled[name] = (
            base,
            field.additional_properties,
            field.alias,
            coercions,
            field.example,
        )
    return MappingProxyType(compiled)


def _lower_alias_metadata(target: type[Any]) -> None:
    """Lower ``Alias`` metadata into the frozen ``Field`` carrier registry reads."""

    from . import Field

    try:
        annotations = get_type_hints(target, include_extras=True)
    except (NameError, TypeError):
        annotations = dict(getattr(target, "__annotations__", {}))
    for name, annotation in annotations.items():
        if get_origin(annotation) is not Annotated:
            continue
        base, *metadata = get_args(annotation)
        aliases = tuple(
            alias for item in metadata if isinstance(item, Alias) for alias in item.names
        )
        if not aliases:
            continue
        fields = [item for item in metadata if isinstance(item, Field)]
        if len(fields) > 1:
            raise TypeError(f"parameter {name!r} carries more than one omp.Field")
        old = fields[0] if fields else Field()
        combined = old.alias + aliases
        if len(set(combined)) != len(combined):
            raise ValueError(f"parameter {name!r} declares a duplicate alias")
        lowered = Field(
            old.description,
            additional_properties=old.additional_properties,
            alias=combined,
            coerce=old.coerce,
            expected=old.expected,
            example=old.example,
        )
        metadata = [lowered if item is old else item for item in metadata]
        if not fields:
            metadata.append(lowered)
        annotations[name] = Annotated[base, *metadata]
    target.__annotations__ = annotations


# ``omp.params`` is both the decorator and the documented constants namespace.
params.MAX_NESTING_DEPTH = MAX_NESTING_DEPTH  # type: ignore[attr-defined]
params.INTERRUPT_GRACE = INTERRUPT_GRACE  # type: ignore[attr-defined]
params.MAX_PENDING_PULLS = MAX_PENDING_PULLS  # type: ignore[attr-defined]


class IncomingParams:
    """Host-constructed linear pull cursor for one invocation argument document."""

    __slots__ = (
        "name",
        "rev",
        "invocation_id",
        "owner",
        "deadline",
        "_phase",
        "_shape",
        "_pending",
        "_interrupts",
        "_interrupt_event",
        "_repairs",
    )

    def __init__(
        self,
        *,
        name: str,
        rev: Rev,
        invocation_id: str,
        owner: str | None = None,
        phase: InvocationPhase = InvocationPhase.OPEN,
        deadline: Duration | None = None,
        shape: type[Any] | None = None,
    ) -> None:
        if not isinstance(name, str) or not name:
            raise TypeError("IncomingParams name must be a non-empty string")
        if not isinstance(rev, Rev):
            raise TypeError("IncomingParams rev must be an omp.Rev")
        if not isinstance(invocation_id, str) or not invocation_id:
            raise TypeError("IncomingParams invocation_id must be a non-empty string")
        if owner is not None and not isinstance(owner, str):
            raise TypeError("IncomingParams owner must be str or None")
        if not isinstance(phase, InvocationPhase):
            raise TypeError("IncomingParams phase must be an InvocationPhase")
        if deadline is not None and not isinstance(deadline, Duration):
            raise TypeError("IncomingParams deadline must be Duration or None")
        if shape is not None and not isinstance(shape, type):
            raise TypeError("IncomingParams shape must be a type or None")
        self.name = name
        self.rev = rev
        self.invocation_id = invocation_id
        self.owner = owner
        self.deadline = deadline
        self._phase = phase
        self._shape = shape
        self._pending = False
        self._interrupts: deque[Interrupt] = deque()
        self._interrupt_event = asyncio.Event()
        self._repairs: list[Repair] = []

    @property
    def phase(self) -> InvocationPhase:
        """Observe the latest monotonic invocation phase."""
        return self._phase

    @property
    def is_authorized(self) -> bool:
        """Return whether the invocation reached effect authorization."""
        return self._phase >= InvocationPhase.EFFECTS_AUTHORIZED

    def arg(
        self,
        name: str,
        *,
        alias: tuple[str, ...] = (),
        coerce: object | tuple[object, ...] | None = None,
        example: str | None = None,
    ) -> Arg:
        """Bind a cheap cursor to one canonical top-level argument."""
        if isinstance(alias, str):
            raise TypeError("argument aliases must be a tuple of strings")
        explicit_aliases = tuple(alias)
        annotation, additional, declared_aliases, declared_coercions, declared_example = (
            _field_shape(self._shape, name)
        )
        effective_example = declared_example if example is None else example
        effective_coerce = declared_coercions if coerce is None else coerce
        aliases, coercions = _pull_options(
            name,
            declared_aliases + explicit_aliases,
            effective_coerce,
            effective_example,
        )
        return Arg(
            self,
            (name,),
            aliases=aliases,
            coercions=coercions,
            example=effective_example,
            declared=annotation,
            additional_properties=additional,
        )

    async def args(self, shape: type[Any] | None = None) -> Any:
        """Wait for strict finalization and decode the canonical argument object."""
        target = self._shape if shape is None else shape
        if target is not None and not isinstance(target, type):
            raise TypeError("IncomingParams.args shape must be a type or None")
        result = await self._request("args", expected=_expected_shape(target))
        return _typed_value(result, target)

    async def raw(self) -> str:
        """Return the exact completed provider emission before repairs."""
        result = await self._request("raw")
        if not isinstance(result, str):
            raise ParamsProtocol("raw pull returned a non-string value")
        return result

    async def committed(self) -> str:
        """Await effect authorization and return canonical effective argument text."""
        result = await self._request("committed")
        if not isinstance(result, str):
            raise ParamsProtocol("commit gate returned a non-string value")
        self._advance(InvocationPhase.EFFECTS_AUTHORIZED)
        return result

    def interruptable(self) -> InterruptibleParams:
        """Return an interrupt-observing view over this same linear cursor."""
        return InterruptibleParams(self)

    def take_interrupt(self) -> Interrupt | None:
        """Remove the oldest queued interrupt without blocking."""
        if not self._interrupts:
            return None
        interrupt = self._interrupts.popleft()
        if not self._interrupts:
            self._interrupt_event.clear()
        return interrupt

    async def next_interrupt(self) -> Interrupt:
        """Wait for and consume the next structured interrupt."""
        queued = self.take_interrupt()
        if queued is not None:
            return queued
        result = await self._request("next_interrupt", counts_as_pull=False)
        if result is None:
            raise InterruptClosed()
        interrupt = _as_interrupt(result)
        return interrupt

    def repairs(self) -> list[Repair]:
        """Return a snapshot of charitable repairs observed so far."""
        return list(self._repairs)

    async def _request(
        self,
        action: str,
        *,
        interruptible: bool = False,
        counts_as_pull: bool = True,
        **arguments: object,
    ) -> Any:
        if counts_as_pull and self._pending:
            raise ParamsMisuse(
                f"at most {MAX_PENDING_PULLS} parameter pull may be pending"
            )
        if interruptible:
            queued = self.take_interrupt()
            if queued is not None:
                raise Interrupted(queued)
        if counts_as_pull:
            self._pending = True
        try:
            request = self._dispatch(
                action, interruptible=interruptible, **arguments
            )
            if not interruptible:
                result = await request
            else:
                result = await self._interruptible_request(request)
            return self._unwrap(result)
        finally:
            if counts_as_pull:
                self._pending = False

    async def _interruptible_request(self, request: Coroutine[Any, Any, Any]) -> Any:
        request_task = asyncio.create_task(request)
        interrupt_task = asyncio.create_task(self._interrupt_event.wait())
        tasks = (request_task, interrupt_task)
        try:
            done, _ = await asyncio.wait(
                tasks, return_when=asyncio.FIRST_COMPLETED
            )
            if request_task in done:
                interrupt_task.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await interrupt_task
                return await request_task
            request_task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await request_task
            interrupt = self.take_interrupt()
            if interrupt is None:
                raise ParamsProtocol("interrupt event fired without an interrupt")
            raise Interrupted(interrupt)
        finally:
            pending = tuple(task for task in tasks if not task.done())
            for task in pending:
                task.cancel()
            if pending:
                await asyncio.gather(*pending, return_exceptions=True)

    async def _dispatch(self, action: str, **arguments: object) -> Any:
        from . import _control_request

        operation = _PARAM_OPERATIONS.get(action)
        if operation is None:
            raise ParamsMisuse(f"unknown parameter cursor operation {action!r}")
        return await _control_request(
            operation,
            invocation_id=self.invocation_id,
            **_json_arguments(arguments),
        )

    def _unwrap(self, result: Any) -> Any:
        if isinstance(result, ArgIssue):
            raise ArgFault(result)
        if isinstance(result, Repair):
            self._repairs.append(result)
            return None
        if isinstance(result, Mapping) and "issue" in result:
            raise ArgFault(_as_issue(result["issue"]))
        if isinstance(result, Mapping) and "aborted" in result:
            reason = result["aborted"]
            if not isinstance(reason, str) or not reason:
                raise ParamsProtocol("host returned an invalid commit abort")
            raise CommitAborted(reason)
        if isinstance(result, Mapping) and "interrupt" in result:
            raise Interrupted(_as_interrupt(result["interrupt"]))
        if isinstance(result, Mapping) and result.get("closed") is True:
            raise InterruptClosed()
        if isinstance(result, Mapping) and "protocol_error" in result:
            detail = result["protocol_error"]
            if not isinstance(detail, str) or not detail:
                raise ParamsProtocol("host returned an invalid protocol error")
            raise ParamsProtocol(detail)
        if isinstance(result, Mapping) and "value" in result:
            repairs = result.get("repairs", ())
            if isinstance(repairs, (str, bytes)) or not isinstance(
                repairs, (list, tuple)
            ):
                raise ParamsProtocol("host returned an invalid repair list")
            for repair in repairs:
                self._repairs.append(_as_repair(repair))
            interrupts = result.get("interrupts", ())
            if isinstance(interrupts, (str, bytes)) or not isinstance(
                interrupts, (list, tuple)
            ):
                raise ParamsProtocol("host returned an invalid interrupt list")
            for interrupt in interrupts:
                self._push_interrupt(_as_interrupt(interrupt))
            phase = result.get("phase")
            if phase is not None:
                self._advance(_as_phase(phase))
            return result["value"]
        if not _is_json_value(result):
            raise ParamsProtocol("host returned a non-JSON cursor result")
        return result

    def _advance(self, phase: InvocationPhase) -> None:
        if phase < self._phase:
            raise ParamsProtocol(
                f"invocation phase regressed from {self._phase.name} to {phase.name}"
            )
        self._phase = phase

    def _push_interrupt(self, interrupt: Interrupt) -> None:
        """Accept a loop-owned interrupt from the host binding."""
        if not isinstance(interrupt, Interrupt):
            raise TypeError("host interrupt must be an Interrupt")
        self._interrupts.append(interrupt)
        self._interrupt_event.set()


class Arg:
    """A one-shot cursor for one path-addressed JSON value."""

    __slots__ = (
        "_params",
        "path",
        "_aliases",
        "_coercions",
        "_example",
        "_declared",
        "_additional_properties",
        "_interruptible",
        "_claimed",
        "_finished",
    )

    def __init__(
        self,
        params: IncomingParams,
        path: tuple[str | int, ...],
        *,
        aliases: tuple[str, ...] = (),
        coercions: tuple[object, ...] = (),
        example: str | None = None,
        declared: object = Any,
        additional_properties: bool = False,
        interruptible: bool = False,
    ) -> None:
        self._params = params
        self.path = path
        self._aliases = aliases
        self._coercions = coercions
        self._example = example
        self._declared = declared
        self._additional_properties = additional_properties
        self._interruptible = interruptible
        self._claimed = False
        self._finished = False

    def __await__(self):
        return self._pull(
            "value", expected=_expected_shape(self._declared)
        ).__await__()

    async def text(self) -> str:
        value = await self._pull("text")
        if not isinstance(value, str):
            self._mismatch("string", value)
        return value

    async def number(self) -> float:
        value = await self._pull("number")
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            self._mismatch("number", value)
        return float(value)

    async def integer(self) -> int:
        value = await self._pull("integer")
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            self._mismatch("integer", value)
        if isinstance(value, float) and not value.is_integer():
            self._mismatch("integer", value)
        return int(value)

    async def boolean(self) -> bool:
        value = await self._pull("boolean")
        if type(value) is not bool:
            self._mismatch("boolean", value)
        return value

    async def null(self) -> None:
        value = await self._pull("null")
        if value is not None:
            self._mismatch("null", value)
        return None

    async def value(self) -> str | int | float | bool | None | list[Any] | dict[str, Any]:
        value = await self._pull("value")
        if not _is_json_value(value):
            raise ParamsProtocol("value pull returned a non-JSON value")
        return value

    async def typed(self, target: type[Any]) -> Any:
        if not isinstance(target, type):
            raise TypeError("Arg.typed target must be a type")
        return _typed_value(
            await self._pull("typed", expected=_expected_shape(target)),
            target,
        )

    async def raw(self) -> str:
        value = await self._pull("raw")
        if not isinstance(value, str):
            raise ParamsProtocol("raw argument pull returned a non-string value")
        return value

    def chunks(self) -> AsyncIterator[str]:
        return self._strings("chunk")

    def lines(self) -> AsyncIterator[str]:
        return self._strings("line")

    def array(self) -> ArgArray:
        self._claim()
        return ArgArray(self)

    def object(self) -> ArgObject:
        self._claim()
        return ArgObject(self, additional_properties=self._additional_properties)

    async def optional(self, default: Any) -> Any:
        try:
            return await self._pull(
                "value",
                optional=True,
                expected=_expected_shape(self._declared),
            )
        except ArgFault as error:
            if error.kind is ArgIssueKind.MISSING:
                return default
            raise

    async def _pull(self, mode: str, **arguments: object) -> Any:
        self._claim()
        try:
            return await self._params._request(
                "pull",
                interruptible=self._interruptible,
                path=self.path,
                mode=mode,
                aliases=self._aliases,
                coercions=self._coercions,
                example=self._example,
                **arguments,
            )
        finally:
            self._finished = True

    async def _strings(self, mode: str) -> AsyncIterator[str]:
        self._claim()
        offset = 0
        try:
            while True:
                value = await self._params._request(
                    "pull",
                    interruptible=self._interruptible,
                    path=self.path,
                    mode=mode,
                    offset=offset,
                    aliases=self._aliases,
                    coercions=self._coercions,
                    example=self._example,
                )
                if value is None:
                    return
                if not isinstance(value, str):
                    raise ParamsProtocol(f"{mode} pull returned a non-string value")
                offset += len(value)
                yield value
        finally:
            self._finished = True

    def _claim(self) -> None:
        if self._claimed:
            raise ParamsMisuse(f"argument cursor at {self.path!r} was already consumed")
        self._claimed = True

    def _mismatch(self, expected: str, found: object) -> None:
        issue = ArgIssue(
            self.path,
            expected,
            ArgIssueKind.TYPE_MISMATCH,
            example=self._example,
            found=_shape_name(found),
        )
        raise ArgFault(issue)


class ArgArray:
    """A linear cursor yielding array elements as each element starts."""

    __slots__ = ("_arg", "_index", "_active", "_closed", "_collecting")

    def __init__(self, arg: Arg) -> None:
        self._arg = arg
        self._index = 0
        self._active: Arg | None = None
        self._closed = False
        self._collecting = False

    @property
    def index(self) -> int:
        return self._index

    def __aiter__(self) -> AsyncIterator[Arg]:
        return self._iterate()

    async def _iterate(self) -> AsyncIterator[Arg]:
        while (element := await self.next()) is not None:
            yield element

    async def next(self) -> Arg | None:
        if self._collecting:
            raise ParamsMisuse("array collect and iteration cannot be mixed")
        if self._closed:
            return None
        if self._active is not None and not self._active._finished:
            raise ParamsMisuse("finish the current array element before advancing")
        present = await self._arg._params._request(
            "array_next",
            interruptible=self._arg._interruptible,
            path=self._arg.path,
            index=self._index,
        )
        if present is None or present is False:
            self._closed = True
            self._active = None
            self._arg._finished = True
            return None
        element = Arg(
            self._arg._params,
            self._arg.path + (self._index,),
            interruptible=self._arg._interruptible,
        )
        self._index += 1
        self._active = element
        return element

    async def collect(self) -> list[Any]:
        if self._index or self._active is not None:
            raise ParamsMisuse("array iteration and collect cannot be mixed")
        self._collecting = True
        value = await self._arg._params._request(
            "pull",
            interruptible=self._arg._interruptible,
            path=self._arg.path,
            mode="array",
        )
        if not isinstance(value, list):
            self._arg._mismatch("array", value)
        self._closed = True
        self._arg._finished = True
        return value


class ArgObject:
    """A linear cursor for declared keys or an explicitly open map."""

    __slots__ = ("_arg", "_additional_properties", "_enumerating", "_collected")

    def __init__(self, arg: Arg, *, additional_properties: bool) -> None:
        self._arg = arg
        self._additional_properties = additional_properties
        self._enumerating = False
        self._collected = False

    def key(
        self,
        name: str,
        *,
        alias: tuple[str, ...] = (),
        coerce: object | tuple[object, ...] | None = None,
        example: str | None = None,
    ) -> Arg:
        if self._collected or self._enumerating:
            raise ParamsMisuse("object cursor has already been consumed")
        aliases, coercions = _pull_options(name, alias, coerce, example)
        return Arg(
            self._arg._params,
            self._arg.path + (name,),
            aliases=aliases,
            coercions=coercions,
            example=example,
            interruptible=self._arg._interruptible,
        )

    async def collect(self) -> dict[str, Any]:
        if self._collected or self._enumerating:
            raise ParamsMisuse("object cursor has already been consumed")
        self._collected = True
        try:
            value = await self._arg._params._request(
                "pull",
                interruptible=self._arg._interruptible,
                path=self._arg.path,
                mode="object",
            )
            if not isinstance(value, dict) or any(not isinstance(key, str) for key in value):
                self._arg._mismatch("object", value)
            return value
        finally:
            self._arg._finished = True

    def keys(self) -> AsyncIterator[tuple[str, Arg]]:
        if not self._additional_properties:
            raise ParamsMisuse(
                "keys() requires a field declared with additional_properties=True"
            )
        if self._collected or self._enumerating:
            raise ParamsMisuse("object cursor has already been consumed")
        self._enumerating = True
        return self._keys()

    async def _keys(self) -> AsyncIterator[tuple[str, Arg]]:
        index = 0
        active: Arg | None = None
        while True:
            if active is not None and not active._finished:
                raise ParamsMisuse("finish the current object member before advancing")
            result = await self._arg._params._request(
                "object_next",
                interruptible=self._arg._interruptible,
                path=self._arg.path,
                index=index,
            )
            if result is None:
                self._arg._finished = True
                return
            if not isinstance(result, str) or not result:
                raise ParamsProtocol("object member pull returned an invalid key")
            active = Arg(
                self._arg._params,
                self._arg.path + (result,),
                interruptible=self._arg._interruptible,
            )
            index += 1
            yield result, active


class InterruptibleParams:
    """An interrupt-observing view over one ``IncomingParams`` cursor."""

    __slots__ = ("_params",)

    def __init__(self, params: IncomingParams) -> None:
        self._params = params

    def arg(
        self,
        name: str,
        *,
        alias: tuple[str, ...] = (),
        coerce: object | tuple[object, ...] | None = None,
        example: str | None = None,
    ) -> Arg:
        arg = self._params.arg(name, alias=alias, coerce=coerce, example=example)
        arg._interruptible = True
        return arg

    async def args(self, shape: type[Any] | None = None) -> Any:
        target = self._params._shape if shape is None else shape
        if target is not None and not isinstance(target, type):
            raise TypeError("InterruptibleParams.args shape must be a type or None")
        result = await self._params._request(
            "args", interruptible=True, expected=_expected_shape(target)
        )
        return _typed_value(result, target)

    async def raw(self) -> str:
        result = await self._params._request("raw", interruptible=True)
        if not isinstance(result, str):
            raise ParamsProtocol("raw pull returned a non-string value")
        return result

    async def committed(self) -> str:
        result = await self._params._request("committed", interruptible=True)
        if not isinstance(result, str):
            raise ParamsProtocol("commit gate returned a non-string value")
        self._params._advance(InvocationPhase.EFFECTS_AUTHORIZED)
        return result


Ev: TypeAlias = Update[Any] | Args | Aborted | Done[Any] | Detached
"""One streaming-device progress or terminal event."""


def _pull_options(
    name: str,
    alias: tuple[str, ...],
    coerce: object | tuple[object, ...] | None,
    example: str | None,
) -> tuple[tuple[str, ...], tuple[object, ...]]:
    from . import Coerce

    if not isinstance(name, str) or not name:
        raise TypeError("argument name must be a non-empty string")
    if isinstance(alias, str):
        raise TypeError("argument aliases must be a tuple of strings")
    aliases = tuple(alias)
    if any(not isinstance(item, str) or not item for item in aliases):
        raise TypeError("argument aliases must be non-empty strings")
    if name in aliases or len(set(aliases)) != len(aliases):
        raise ValueError("canonical name and aliases must be unique")
    if coerce is None:
        coercions: tuple[object, ...] = ()
    elif isinstance(coerce, Coerce):
        coercions = (coerce,)
    else:
        coercions = tuple(coerce)
    if any(not isinstance(item, Coerce) for item in coercions):
        raise TypeError("coerce must contain only omp.Coerce members")
    if example is not None and not isinstance(example, str):
        raise TypeError("argument example must be str or None")
    return aliases, coercions


def _field_shape(
    shape: type[Any] | None, name: str
) -> tuple[object, bool, tuple[str, ...], tuple[object, ...], str | None]:
    if shape is None:
        return Any, False, (), (), None
    compiled = getattr(shape, "__omp_param_fields__", None)
    if isinstance(compiled, Mapping):
        return compiled.get(name, (Any, False, (), (), None))
    annotation = getattr(shape, "__annotations__", {}).get(name, Any)
    additional = False
    aliases: tuple[str, ...] = ()
    coercions: tuple[object, ...] = ()
    example: str | None = None
    if get_origin(annotation) is Annotated:
        from . import Coerce, Field

        base, *metadata = get_args(annotation)
        annotation = base
        fields = tuple(item for item in metadata if isinstance(item, Field))
        field = fields[0] if fields else Field()
        additional = field.additional_properties
        aliases = field.alias
        coercions = field.coerce + tuple(
            item for item in metadata if isinstance(item, Coerce)
        )
        example = field.example
    return annotation, additional, aliases, coercions, example


def _typed_value(value: Any, target: object) -> Any:
    if target is None or target is Any:
        return value
    if not isinstance(target, type):
        return value
    if isinstance(value, target):
        return value
    try:
        if dataclasses.is_dataclass(target) and isinstance(value, Mapping):
            return target(**value)
        return target(value)
    except (TypeError, ValueError) as error:
        raise ArgFault((), ArgIssueKind.MALFORMED, str(error)) from error


def _as_issue(value: object) -> ArgIssue:
    if isinstance(value, ArgIssue):
        return value
    if not isinstance(value, Mapping):
        raise ParamsProtocol("host returned an invalid argument issue")
    try:
        return ArgIssue(
            tuple(value.get("path", ())),
            value["expected"],
            ArgIssueKind(value["kind"]),
            example=value.get("example"),
            found=value.get("found"),
        )
    except (KeyError, TypeError, ValueError) as error:
        raise ParamsProtocol("host returned an invalid argument issue") from error


def _as_repair(value: object) -> Repair:
    if isinstance(value, Repair):
        return value
    if not isinstance(value, Mapping):
        raise ParamsProtocol("host returned an invalid repair")
    try:
        return Repair(
            tuple(value.get("path", ())),
            RepairKind(value["kind"]),
            value["detail"],
        )
    except (KeyError, TypeError, ValueError) as error:
        raise ParamsProtocol("host returned an invalid repair") from error


def _as_interrupt(value: object) -> Interrupt:
    if isinstance(value, Interrupt):
        return value
    if isinstance(value, Mapping):
        try:
            return Interrupt(value["kind"], value["reason"])
        except (KeyError, TypeError) as error:
            raise ParamsProtocol("host returned an invalid interrupt") from error
    raise ParamsProtocol("host returned an invalid interrupt")


def _as_phase(value: object) -> InvocationPhase:
    if isinstance(value, InvocationPhase):
        return value
    phases = (
        InvocationPhase.OPEN,
        InvocationPhase.ARGS_FINALIZED,
        InvocationPhase.ADMISSION,
        InvocationPhase.ADMITTED,
        InvocationPhase.ASSISTANT_ITEM_COMMITTED,
        InvocationPhase.EFFECTS_AUTHORIZED,
        InvocationPhase.SETTLED,
    )
    if isinstance(value, str):
        normalized = value.upper()
        for phase in phases:
            if phase.value.upper() == normalized:
                return phase
    elif isinstance(value, int) and not isinstance(value, bool):
        for phase in phases:
            if phase.ordinal == value:
                return phase
    raise ParamsProtocol("host returned an invalid invocation phase")


def _expected_shape(target: object) -> str:
    if target is None or target is Any:
        return "any JSON value"
    origin = get_origin(target)
    if origin is not None:
        return str(target)
    names = {
        str: "string",
        int: "integer",
        float: "number",
        bool: "boolean",
        list: "array",
        dict: "object",
        type(None): "null",
    }
    if target in names:
        return names[target]
    return getattr(target, "__name__", str(target))


def _json_arguments(arguments: Mapping[str, object]) -> dict[str, object]:
    """Lower cursor metadata to the shared JSON CONTROL vocabulary."""

    lowered: dict[str, object] = {}
    for key, value in arguments.items():
        if key == "path" or key == "aliases":
            value = list(value)  # type: ignore[arg-type]
        elif key == "coercions":
            value = [item.value for item in value]  # type: ignore[union-attr]
        if not _is_json_value(value):
            raise ParamsMisuse(f"{key} is not JSON-serializable cursor metadata")
        lowered[key] = value
    return lowered


def _shape_name(value: object) -> str:
    if value is None:
        return "null"
    if type(value) is bool:
        return "boolean"
    if isinstance(value, str):
        return "string"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    return type(value).__name__


def _is_json_value(value: object) -> bool:
    if value is None or type(value) in (str, int, float, bool):
        return True
    if isinstance(value, list):
        return all(_is_json_value(item) for item in value)
    if isinstance(value, dict):
        return all(isinstance(key, str) and _is_json_value(item) for key, item in value.items())
    return False


__all__ = (
    "Abort",
    "Alias",
    "Arg",
    "ArgArray",
    "ArgFault",
    "ArgIssue",
    "ArgIssueKind",
    "ArgObject",
    "Args",
    "CommitAborted",
    "Ev",
    "IncomingParams",
    "Interrupt",
    "InterruptClosed",
    "Interrupted",
    "InterruptibleParams",
    "InvocationEnded",
    "INTERRUPT_GRACE",
    "MAX_NESTING_DEPTH",
    "MAX_PENDING_PULLS",
    "ParamsMisuse",
    "ParamsProtocol",
    "Repair",
    "RepairKind",
    "params",
)
