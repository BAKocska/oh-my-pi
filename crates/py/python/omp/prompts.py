"""Frozen prompt-slot declarations and invalidation namespace."""

from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
from typing import Any

from _omp import OmpError

from ._errors import NotWiredError
from ._registry import registry as _declarations


class SlotClass(StrEnum):
    """Stability band governing prompt-prefix cache placement."""

    FROZEN = "frozen"
    STABLE = "stable"
    EPOCHAL = "epochal"
    VOLATILE = "volatile"


@dataclass(frozen=True, slots=True)
class PromptContext:
    """Immutable input supplied to a pure prompt-slot renderer."""

    session_id: str
    model: str
    provider: str
    context_window: int
    epoch: int
    cwd: str
    roots: tuple[str, ...]
    vcs_branch: str | None
    vcs_commit: str | None
    is_subagent: bool
    agent_kind: str | None
    slot: str
    cls: SlotClass
    budget_bytes: int


class UnknownSlot(ValueError):
    """A prompt-slot declaration names a non-writable catalog slot."""


class SlotClassConflict(ValueError):
    """A contribution attempts to loosen its catalog stability band."""


class VolatilePrompt(OmpError):
    """A slot function returned different bytes on the harness's two renders."""


_SLOT_CLASSES = {
    "runtime": SlotClass.FROZEN,
    "policy": SlotClass.STABLE,
    "workflow": SlotClass.FROZEN,
    "skills": SlotClass.STABLE,
    "rules": SlotClass.STABLE,
    "guidance": SlotClass.STABLE,
    "workspace": SlotClass.STABLE,
    "memory": SlotClass.EPOCHAL,
    "standing": SlotClass.EPOCHAL,
    "recall": SlotClass.VOLATILE,
    "status": SlotClass.VOLATILE,
}
_CLASS_RANK = {
    SlotClass.FROZEN: 0,
    SlotClass.STABLE: 1,
    SlotClass.EPOCHAL: 2,
    SlotClass.VOLATILE: 3,
}


def prompt_slot(slot: str, *, priority: int = 0, cls: SlotClass | None = None):
    """Declare a synchronous, pure contribution to a writable prompt slot."""

    try:
        default_cls = _SLOT_CLASSES[slot]
    except KeyError as error:
        raise UnknownSlot(f"unknown or non-writable prompt slot: {slot!r}") from error
    parsed_cls = default_cls if cls is None else SlotClass(cls)
    if _CLASS_RANK[parsed_cls] > _CLASS_RANK[default_cls]:
        raise SlotClassConflict(
            f"{slot!r} is {default_cls.value}; {parsed_cls.value} would loosen it"
        )
    if not isinstance(priority, int):
        raise TypeError("prompt-slot priority must be an int")

    def decorate(function: Any) -> Any:
        if not callable(function):
            raise TypeError("@omp.prompt_slot may decorate only a callable")
        _declarations.register_prompt_slot(slot, priority, parsed_cls, function)
        return function

    return decorate


async def invalidate(slot: str) -> int:
    """Invalidate this extension's contribution through the future prompt host arm."""

    if slot not in _SLOT_CLASSES:
        raise UnknownSlot(f"unknown or non-writable prompt slot: {slot!r}")
    raise NotWiredError("omp.prompts.invalidate")


__all__ = (
    "PromptContext", "SlotClass", "SlotClassConflict", "UnknownSlot", "VolatilePrompt",
    "invalidate", "prompt_slot",
)
