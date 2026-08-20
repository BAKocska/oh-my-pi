"""Per-invocation authority scopes; inert at import."""
from __future__ import annotations
import contextvars
from dataclasses import dataclass
from typing import Any

@dataclass(frozen=True, slots=True)
class Scope:
    """The generation-fenced authority attached to one invocation."""
    invocation: str
    generation: int
    effects: frozenset[str] = frozenset()

_current: contextvars.ContextVar[Scope | None] = contextvars.ContextVar("omp_scope", default=None)

def current() -> Scope:
    """Return the active invocation scope."""
    scope = _current.get()
    if scope is None:
        raise RuntimeError("no active omp invocation scope")
    return scope

def install(scope: Scope) -> contextvars.Token[Scope | None]:
    """Install a scope for host dispatch and return its reset token."""
    return _current.set(scope)

def reset(token: contextvars.Token[Scope | None]) -> None:
    """Restore the scope preceding ``install``."""
    _current.reset(token)
