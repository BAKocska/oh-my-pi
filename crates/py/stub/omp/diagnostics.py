"""Stable deployment diagnostics shared by production omp and omp-stub."""
from enum import StrEnum


class FailureCode(StrEnum):
    """Deployment failures exposed on the omp wire."""

    UNSAT = "E-UNSAT"
    FROZEN_CONFLICT = "E-FROZEN-CONFLICT"
    LOCK_PYTHON = "E-LOCK-PYTHON"
    REVOKED = "E-REVOKED"
    ABI_EXPORT = "E-ABI-EXPORT"
    REPLACE_SCOPE = "E-REPLACE-SCOPE"
    TRUSTED_LOAD = "E-TRUSTED-LOAD"
    SETTING_SECRET = "E-SETTING-SECRET"


class WarningCode(StrEnum):
    """Deployment warnings exposed on the omp wire."""

    YANKED = "W-YANKED"
    SITE_OVERRIDE = "W-SITE-OVERRIDE"
    API_SKEW = "W-API-SKEW"
    FOREIGN_ROOT = "W-FOREIGN-ROOT"
    REPLACE_DENIED = "W-REPLACE-DENIED"
    POOL_COUNT = "W-POOL-COUNT"


__all__ = ("FailureCode", "WarningCode")
