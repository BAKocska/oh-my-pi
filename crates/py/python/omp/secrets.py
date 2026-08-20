"""Declare secret rules while keeping masking entirely inside Core.

The extension declares rules, and Core applies them to host-owned bytes. Secret
bytes never round-trip through Python for masking; until the host arms are
installed, both operations fail closed with :class:`omp.NotWiredError`.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from enum import StrEnum

from ._errors import NotWiredError


_MASKED_PLACEHOLDER = re.compile(
    r"\$\$(?:[A-Z][A-Z0-9_]*_)?[0-9a-z]{12}(?::[ULCM])?\$\$"
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


def declare(rule: SecretRule) -> None:
    """Declare one secret rule through the host-owned declaration arm."""

    del rule
    raise NotWiredError("omp.secrets.declare")


def mask(text: str) -> str:
    """Return Core's masked projection without exposing secret bytes to Python."""

    del text
    raise NotWiredError("omp.secrets.mask")


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
