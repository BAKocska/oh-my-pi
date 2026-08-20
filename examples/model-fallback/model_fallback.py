"""Choose explicit model failover from Core-classified provider errors."""

from __future__ import annotations

import omp
from omp import ErrorKind, Failover, ModelFallback, ProviderError, Retryability

_SHORT_RATE_LIMIT = omp.Duration("20s")
_DEFAULT_COOLDOWN = omp.Duration("1h")
_RETRYABLE_TRANSIENT = frozenset(
    {
        ErrorKind.RESOURCE_EXHAUSTED,
        ErrorKind.CONNECTIVITY,
        ErrorKind.STREAM_CORRUPTION,
    }
)
_SAME_ATTEMPT = frozenset(
    {
        Retryability.SAME_ROUTE,
        Retryability.AFTER_DELAY,
    }
)


@omp.hook("provider_error")
async def fallback(err: ProviderError, ctx: omp.Context) -> Failover | None:
    """Map typed provider failure evidence to a bounded recovery decision."""
    if err.committed:
        return None

    if err.kind is ErrorKind.RATE_LIMITED:
        if (
            err.retryability in _SAME_ATTEMPT
            and err.retry_after is not None
            and err.retry_after <= _SHORT_RATE_LIMIT
        ):
            return Failover.retry(after=err.retry_after)
        return _next_model(err, ctx, cooldown=err.retry_after or _DEFAULT_COOLDOWN)

    if err.kind is ErrorKind.QUOTA_EXHAUSTED:
        return _next_model(err, ctx, cooldown=_DEFAULT_COOLDOWN)

    if (
        err.kind is ErrorKind.AUTHENTICATION
        and err.retryability is Retryability.AFTER_CREDENTIAL
    ):
        return Failover.refresh_credential()

    if err.kind is ErrorKind.CONTEXT_OVERFLOW:
        return None

    if (
        err.kind in _RETRYABLE_TRANSIENT
        and err.retryability in _SAME_ATTEMPT
        and err.attempt < 3
    ):
        after = err.retry_after or omp.Duration(f"{2 * err.attempt}s")
        return Failover.retry(after=after)

    return None


def _next_model(
    err: ProviderError,
    ctx: omp.Context,
    *,
    cooldown: omp.Duration,
) -> Failover | None:
    """Switch only to the next target in the explicitly declared chain."""
    policy = ModelFallback(ctx.settings.get("model_fallback", ModelFallback.DENY))
    if policy is not ModelFallback.CHAIN:
        return None

    chain = tuple(
        target.strip()
        for target in ctx.settings.get("fallback_chain", "").split(",")
        if target.strip()
    )
    current = f"{err.provider}/{err.model}"
    if current not in chain:
        return None
    index = chain.index(current) + 1
    if index >= len(chain):
        return None
    return Failover.switch_model(chain[index], cooldown=cooldown)
