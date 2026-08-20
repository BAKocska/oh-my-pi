"""Immutable public view of the active invocation scope."""

from __future__ import annotations

import time
from dataclasses import dataclass

from _omp import Duration, InvocationPhase, Principal

from . import _scope


@dataclass(frozen=True, slots=True)
class Context:
    """Invocation identity and authority metadata supplied to every callback."""

    invocation: str
    principal: Principal
    generation: int
    phase: InvocationPhase
    deadline: float | None = None

    @classmethod
    def from_scope(cls, scope: _scope.Scope) -> Context:
        """Project a host-owned authority scope into its immutable public view."""
        return cls(
            invocation=scope.invocation,
            principal=scope.principal,
            generation=scope.generation,
            phase=scope.phase,
            deadline=scope.deadline,
        )

    @classmethod
    def current(cls) -> Context:
        """Return the active callback context, or raise ``LookupError`` outside one."""
        try:
            scope = _scope.current()
        except RuntimeError as error:
            raise LookupError("no active omp invocation context") from error
        return cls.from_scope(scope)

    def deadline_in(self) -> Duration | None:
        """Return remaining time as a typed duration, clamped at zero."""
        if self.deadline is None:
            return None
        return Duration(seconds=max(0.0, self.deadline - time.monotonic()))
