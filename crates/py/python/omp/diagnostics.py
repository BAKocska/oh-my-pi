"""Stable deployment diagnostic vocabulary.

Values mirror the resolver's ``strum`` vocabulary so Python extensions can branch on
wire-stable diagnostics without parsing human-readable messages.  Defining these
values performs no host interaction.
"""
from __future__ import annotations

from enum import StrEnum


class FailureCode(StrEnum):
    """Deployment failures that prevent the requested resolution or load."""

    UNSAT = "E-UNSAT"
    FROZEN_CONFLICT = "E-FROZEN-CONFLICT"
    LOCK_PYTHON = "E-LOCK-PYTHON"
    REVOKED = "E-REVOKED"
    ABI_EXPORT = "E-ABI-EXPORT"
    REPLACE_SCOPE = "E-REPLACE-SCOPE"
    TRUSTED_LOAD = "E-TRUSTED-LOAD"
    SETTING_SECRET = "E-SETTING-SECRET"


class WarningCode(StrEnum):
    """Deployment warnings that preserve a usable, but notable, outcome."""

    YANKED = "W-YANKED"
    SITE_OVERRIDE = "W-SITE-OVERRIDE"
    API_SKEW = "W-API-SKEW"
    FOREIGN_ROOT = "W-FOREIGN-ROOT"
    REPLACE_DENIED = "W-REPLACE-DENIED"
    POOL_COUNT = "W-POOL-COUNT"


class DiagnosticCode(StrEnum):
    """Known failure and warning diagnostic codes.

    Use :class:`FailureCode` or :class:`WarningCode` where the severity is known.
    This union is useful when decoding an untyped diagnostic frame.
    """

    UNSAT = FailureCode.UNSAT
    FROZEN_CONFLICT = FailureCode.FROZEN_CONFLICT
    LOCK_PYTHON = FailureCode.LOCK_PYTHON
    REVOKED = FailureCode.REVOKED
    ABI_EXPORT = FailureCode.ABI_EXPORT
    REPLACE_SCOPE = FailureCode.REPLACE_SCOPE
    TRUSTED_LOAD = FailureCode.TRUSTED_LOAD
    SETTING_SECRET = FailureCode.SETTING_SECRET
    YANKED = WarningCode.YANKED
    SITE_OVERRIDE = WarningCode.SITE_OVERRIDE
    API_SKEW = WarningCode.API_SKEW
    FOREIGN_ROOT = WarningCode.FOREIGN_ROOT
    REPLACE_DENIED = WarningCode.REPLACE_DENIED
    POOL_COUNT = WarningCode.POOL_COUNT


__all__ = ("DiagnosticCode", "FailureCode", "WarningCode")
