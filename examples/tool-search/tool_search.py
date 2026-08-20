from __future__ import annotations

from dataclasses import dataclass
from typing import Any

import omp


_DISABLED_REASON = "disabled by tool-search"


@dataclass(frozen=True, slots=True)
class ToggleArgs:
    """Name one manifest-allowed device whose availability should change."""

    name: str


def _allowlist(ctx: omp.Context) -> frozenset[str]:
    value = ctx.settings.get("allowlist", "")
    if not isinstance(value, str):
        return frozenset()
    return frozenset(name.strip() for name in value.split(",") if name.strip())


def _refused(name: str, reason: str) -> dict[str, object]:
    return {"changed": False, "name": name, "status": "refused", "reason": reason}


async def _set_availability(
    args: ToggleArgs, ctx: omp.Context, *, mounted: bool
) -> dict[str, object]:
    name = args.name.strip()
    if not name or name != args.name:
        return _refused(args.name, "name must be a non-empty canonical device path")
    if name not in _allowlist(ctx):
        return _refused(name, "device is not in this extension's settings allowlist")

    rows = await omp.devices.list(mounted_only=False)
    row: Any | None = next(
        (
            item
            for item in rows
            if str(item.path) == name and item.shadowed_by is None
        ),
        None,
    )
    if row is None:
        return _refused(name, "device is not present in the session catalog")
    if row.slotted:
        return _refused(
            name,
            "model-facing hard slots require an install-time tools.hard grant",
        )
    if row.mounted is mounted:
        return {
            "changed": False,
            "name": name,
            "status": "already_enabled" if mounted else "already_disabled",
        }

    delta = omp.AvailabilityDelta(
        path=name,
        mounted=mounted,
        reason=None if mounted else _DISABLED_REASON,
    )
    await omp.devices.set_availability(delta)
    return {
        "changed": True,
        "name": name,
        "status": "enabled" if mounted else "disabled",
        "delivery": "TurnBoundary",
    }


@omp.device("tool_enable", family="tool-search", rev=1)
async def tool_enable(args: ToggleArgs, ctx: omp.Context) -> dict[str, object]:
    """Make one allowlisted soft device available at the next turn boundary."""

    return await _set_availability(args, ctx, mounted=True)


@omp.device("tool_disable", family="tool-search", rev=1)
async def tool_disable(args: ToggleArgs, ctx: omp.Context) -> dict[str, object]:
    """Make one allowlisted soft device unavailable at the next turn boundary."""

    return await _set_availability(args, ctx, mounted=False)
