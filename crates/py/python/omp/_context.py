"""Immutable public view of the active invocation scope."""

from __future__ import annotations

import asyncio
import contextvars
import time
from collections.abc import AsyncIterator, Callable, Mapping
from contextlib import asynccontextmanager
from dataclasses import dataclass, field, replace
from types import MappingProxyType
from typing import Any

from _omp import CapabilityError, Duration, LifecyclePhase, Principal, WorkspaceUri

from . import _scope
from .placement import Place
from .provider import ModelRef


_EMPTY_SETTINGS: Mapping[str, object] = MappingProxyType({})
_log_sink: contextvars.ContextVar[Callable[..., None] | None] = contextvars.ContextVar(
    "omp_context_log_sink", default=None
)


def _install_log_sink(sink: Callable[..., None] | None) -> None:
    """Install the host-owned synchronous structured-log sink for this context."""
    _log_sink.set(sink)


@dataclass(frozen=True, slots=True)
class Context:
    """Immutable public projection of a host-owned callback scope."""

    extension: str
    session: str
    invocation: str
    principal: Principal
    generation: int
    turn: int | None = None
    event: str | None = None
    call: str | None = None
    device: str | None = None
    trust: _scope.Trust = _scope.Trust.SANDBOXED
    caps: frozenset[str] = frozenset()
    place: Place = Place.HOST
    phase: LifecyclePhase = LifecyclePhase.ACTIVE
    roots: tuple[WorkspaceUri, ...] = ()
    remote: bool = False
    has_ui: bool = False
    headless: bool = True
    model: ModelRef | None = None
    settings: Mapping[str, object] = field(default_factory=lambda: _EMPTY_SETTINGS)
    deadline: float | None = None
    _scope: _scope.Scope | None = field(default=None, repr=False, compare=False)

    @classmethod
    def from_scope(cls, scope: _scope.Scope) -> Context:
        """Project a host-owned authority scope into its immutable public view."""
        return cls(
            extension=scope.extension,
            session=scope.session,
            invocation=scope.invocation,
            principal=scope.principal,
            generation=scope.generation,
            turn=scope.turn,
            event=scope.event,
            call=scope.call,
            device=scope.device,
            trust=scope.trust,
            caps=scope.caps,
            place=Place.parse(scope.place_kind),
            phase=scope.lifecycle,
            roots=tuple(WorkspaceUri(root) for root in scope.roots),
            remote=scope.remote,
            has_ui=scope.has_ui,
            headless=scope.headless,
            model=scope.model,
            settings=scope.settings,
            deadline=scope.deadline,
            _scope=scope,
        )

    @classmethod
    def current(cls) -> Context:
        """Return the active callback context, or raise ``LookupError`` outside one."""
        try:
            scope = _scope.current()
        except RuntimeError as error:
            raise LookupError("no active omp invocation context") from error
        return cls.from_scope(scope)

    @property
    def root(self) -> WorkspaceUri:
        """Return the primary workspace root."""
        if not self.roots:
            raise LookupError("no workspace roots")
        return self.roots[0]

    def deadline_in(self) -> Duration | None:
        """Return remaining time as a typed duration, clamped at zero."""
        if self.deadline is None:
            return None
        return Duration(seconds=max(0.0, self.deadline - time.monotonic()))

    def cancelled(self) -> bool:
        """Return whether cancellation has been requested for this scope."""
        event = self._scope.cancelled if self._scope is not None else None
        return bool(event is not None and event.is_set())

    def checkpoint(self) -> None:
        """Raise ``CancelledError`` when cancellation is pending."""
        if self.cancelled():
            raise asyncio.CancelledError

    def on_cancel(self, fn: Callable[[], None]) -> Callable[[], None]:
        """Register a synchronous cancellation callback and return its remover."""
        if self._scope is None:
            raise RuntimeError("context is not attached to an invocation scope")
        callbacks = self._scope.cancel_callbacks
        callbacks.append(fn)

        def unregister() -> None:
            try:
                callbacks.remove(fn)
            except ValueError:
                pass

        return unregister

    @asynccontextmanager
    async def shield(self) -> AsyncIterator[None]:
        """Defer cooperative cancellation delivery across a critical section."""
        token = _scope._shielded.set(True)
        try:
            yield
        finally:
            _scope._shielded.reset(token)

    def require(self, *caps: Any) -> None:
        """Raise ``CapabilityError`` naming capabilities absent from this scope."""
        requested = tuple(str(getattr(cap, "value", cap)) for cap in caps)
        missing = tuple(cap for cap in requested if cap not in self.caps)
        if missing:
            raise CapabilityError(
                f"required capabilities not granted: {', '.join(missing)}"
            )

    def log(self, level: object, message: str, /, **fields: object) -> None:
        """Emit a redacted structured log to the installed host sink."""
        sink = _log_sink.get()
        if sink is None:
            return
        redacted = dict(fields)
        if self._scope is not None:
            for key in self._scope.secret_settings:
                if key in redacted:
                    redacted[key] = "[REDACTED]"
        redacted.update(
            extension=self.extension,
            session=self.session,
            generation=self.generation,
        )
        if self.event is not None:
            redacted["event"] = self.event
        if self.call is not None:
            redacted["call"] = self.call
        try:
            sink(level, message, redacted)
        except Exception:
            pass

    def child(self, **overrides: object) -> Context:
        """Return an immutable derived context with the requested overrides."""
        return replace(self, **overrides)
