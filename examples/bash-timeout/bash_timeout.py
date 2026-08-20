"""Clamp excessive core bash timeouts before admission."""

from __future__ import annotations

import omp

_DEFAULT_MAX_TIMEOUT = "10m"


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.TRANSFORM,
    order=100,
    when=omp.When(name={"bash"}),
)
async def clamp_bash_timeout(event, ctx):
    """Clamp bash timeouts to the extension's configured maximum."""
    ceiling = omp.Duration(ctx.settings.get("max_timeout", _DEFAULT_MAX_TIMEOUT))
    timeout = event.args.get("timeout")
    if timeout is None or timeout <= ceiling:
        return None
    return omp.Modify(
        patch={"timeout": ceiling},
        reason=f"clamped timeout {timeout} to {ceiling}",
    )
