"""Declare secret rules while keeping masking entirely inside Core.

The extension declares rules, and Core applies them to host-owned bytes. Secret
bytes never round-trip through Python for masking; until the host arms are
installed, both operations fail closed with :class:`omp.NotWiredError`.
"""

from __future__ import annotations

import re
import contextvars
from dataclasses import dataclass
from enum import StrEnum

from ._errors import NotWiredError


_MASKED_PLACEHOLDER = re.compile(
    r"\$\$(?:[A-Z0-9]+_)?[A-Z0-9]{12}(?::[ULCM])?\$\$"
)


class SecretKind(StrEnum):
    """Select how Core resolves a secret rule's pattern."""

    LITERAL = "literal"
    REGEX = "regex"
    ENV = "env"


class SecretMode(StrEnum):
    """Select whether Core masks a secret reversibly or permanently."""

    OBFUSCATE = "obfuscate"
    REDACT = "redact"


@dataclass(frozen=True, slots=True)
class SecretRule:
    """Declare one Core-owned secret matching and masking rule."""

    pattern: str
    kind: SecretKind = SecretKind.LITERAL
    mode: SecretMode = SecretMode.OBFUSCATE
    label: str = ""
    replacement: str | None = None
    flags: str | None = None


_backend: contextvars.ContextVar[object | None] = contextvars.ContextVar(
    "omp_secrets_backend", default=None
)


def _install_backend(backend: object | None) -> None:
    """Install Core's invocation-scoped secret declaration and masking bridge."""
    _backend.set(backend)


def _wire_rule(rule: SecretRule) -> dict[str, object]:
    if not isinstance(rule, SecretRule):
        raise TypeError("declare expects a SecretRule")
    kind = {
        SecretKind.LITERAL: "plain",
        SecretKind.REGEX: "regex",
        SecretKind.ENV: "env",
    }[rule.kind]
    mode = {
        SecretMode.OBFUSCATE: "obfuscate",
        SecretMode.REDACT: "replace",
    }[rule.mode]
    return {
        "kind": kind,
        "mode": mode,
        "content": rule.pattern,
        "friendly_name": rule.label or None,
        "replacement": rule.replacement,
        "flags": rule.flags,
    }


def declare(rule: SecretRule) -> None:
    """Declare one secret rule through the host-owned declaration arm."""

    backend = _backend.get()
    declare_secret = None if backend is None else getattr(backend, "declare_secret", None)
    if declare_secret is None:
        raise NotWiredError("omp.secrets.declare")
    declare_secret(_wire_rule(rule))


def mask(text: str) -> str:
    """Return Core's masked projection without exposing secret bytes to Python."""

    if not isinstance(text, str):
        raise TypeError("mask expects a string")
    backend = _backend.get()
    mask_secret = None if backend is None else getattr(backend, "mask_secret", None)
    if mask_secret is None:
        raise NotWiredError("omp.secrets.mask")
    masked = mask_secret(text)
    if not isinstance(masked, str):
        raise TypeError("Core secret mask returned a non-string value")
    return masked


def is_masked(text: str) -> bool:
    """Return whether text contains a canonical reversible secret placeholder."""
    if not isinstance(text, str):
        raise TypeError("is_masked expects a string")
    return _MASKED_PLACEHOLDER.search(text) is not None


__all__ = (
    "SecretKind",
    "SecretMode",
    "SecretRule",
    "declare",
    "is_masked",
    "mask",
)
