"""Frozen core-enforced ceilings."""

from __future__ import annotations

from typing import Final

from _omp import Duration

REENTRANCY_DEPTH: Final[int] = 4
INTERACTIVE_CAP: Final[Duration] = Duration("15m")
SETTLE_CONTINUATION_CAP: Final[int] = 8
SHUTDOWN_BUDGET: Final[Duration] = Duration("2s")
OBSERVE_CAP: Final[int] = 64
MODIFY_ROUNDS: Final[int] = 1

__all__ = (
    "INTERACTIVE_CAP",
    "MODIFY_ROUNDS",
    "OBSERVE_CAP",
    "REENTRANCY_DEPTH",
    "SETTLE_CONTINUATION_CAP",
    "SHUTDOWN_BUDGET",
)
