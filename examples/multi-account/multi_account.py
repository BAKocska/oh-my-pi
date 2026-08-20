"""Rotate among manifest-scoped credentials without handling their secrets."""

from __future__ import annotations

import asyncio
from collections.abc import Mapping, Sequence
from dataclasses import dataclass

import omp
from omp import ErrorKind, Failover, Payload, ProviderError

_DEFAULT_RATE_COOLDOWN = "15m"
_DEFAULT_QUOTA_COOLDOWN = "6h"
_ROTATABLE = frozenset({ErrorKind.RATE_LIMITED, ErrorKind.QUOTA_EXHAUSTED})


@dataclass(frozen=True, slots=True)
class AccountScope:
    """Name one configured provider credential identity."""

    provider: str
    identity: str


@dataclass(frozen=True, slots=True)
class AccountsArgs:
    """Optionally restrict account status to one configured provider."""

    provider: str | None = None


@dataclass(frozen=True, slots=True)
class BlockStatus:
    """Project one secret-free durable credential block receipt."""

    scope: str | None
    until_ms: int | None


@dataclass(frozen=True, slots=True)
class AccountStatus:
    """Describe one configured account's position and persisted eligibility state."""

    position: int
    provider: str
    identity: str
    next_identity: str | None
    state: str
    credential_id: int | None
    expires_at_ms: int | None
    blocks: tuple[BlockStatus, ...]


@dataclass(frozen=True, slots=True)
class AccountsPayload(Payload):
    """Return configured rotation order with host-owned credential state."""

    accounts: tuple[AccountStatus, ...]


@dataclass(frozen=True, slots=True)
class AccountsFault(omp.Fault):
    """Report an invalid account declaration or provider filter."""

    detail: str


def _account_order(raw: object) -> tuple[AccountScope, ...]:
    """Parse ordered ``provider:identity`` settings without accepting secrets."""

    scopes: list[AccountScope] = []
    seen: set[tuple[str, str]] = set()
    for item in str(raw).split(","):
        declaration = item.strip()
        if not declaration:
            continue
        provider, separator, identity = declaration.partition(":")
        provider = provider.strip()
        identity = identity.strip()
        if not separator or not provider or not identity:
            raise ValueError("accounts must be comma-separated provider:identity pairs")
        key = (provider, identity)
        if key in seen:
            raise ValueError(f"duplicate account scope {provider}:{identity}")
        seen.add(key)
        scopes.append(AccountScope(provider, identity))
    return tuple(scopes)


def _next_scope(
    current: AccountScope, accounts: Sequence[AccountScope]
) -> AccountScope | None:
    """Find the configured successor within the current provider's account pool."""

    provider_accounts = tuple(account for account in accounts if account.provider == current.provider)
    if len(provider_accounts) < 2 or current not in provider_accounts:
        return None
    index = provider_accounts.index(current)
    return provider_accounts[(index + 1) % len(provider_accounts)]


def _rotation_decision(
    err: ProviderError,
    accounts: Sequence[AccountScope],
    *,
    rate_cooldown: omp.Duration,
    quota_cooldown: omp.Duration,
) -> tuple[AccountScope, Failover] | None:
    """Choose a successor and ask Core to cool and rotate the current identity."""

    if err.committed or err.kind not in _ROTATABLE or err.identity is None:
        return None
    current = AccountScope(err.provider, err.identity)
    successor = _next_scope(current, accounts)
    if successor is None:
        return None
    cooldown = (
        err.retry_after or rate_cooldown
        if err.kind is ErrorKind.RATE_LIMITED
        else quota_cooldown
    )
    return successor, Failover.rotate_account(cooldown=cooldown)


@omp.hook("provider_error")
async def rotate_account(err: ProviderError, ctx: omp.Context) -> Failover | None:
    """Rotate on typed quota evidence while Core persists identity cooldowns."""

    accounts = _account_order(ctx.settings.get("accounts", ""))
    decision = _rotation_decision(
        err,
        accounts,
        rate_cooldown=omp.Duration(
            str(ctx.settings.get("rate_limit_cooldown", _DEFAULT_RATE_COOLDOWN))
        ),
        quota_cooldown=omp.Duration(
            str(ctx.settings.get("quota_cooldown", _DEFAULT_QUOTA_COOLDOWN))
        ),
    )
    return decision[1] if decision is not None else None


def _optional_int(value: object) -> int | None:
    """Accept an integer receipt timestamp without coercing arbitrary values."""

    return value if isinstance(value, int) and not isinstance(value, bool) else None


def _optional_text(value: object) -> str | None:
    """Accept a textual receipt scope without formatting arbitrary values."""

    return value if isinstance(value, str) else None


def _block_status(block: Mapping[str, object]) -> BlockStatus:
    """Keep only non-secret block scope and expiry fields in tool output."""

    until_ms = _optional_int(block.get("until_ms"))
    if until_ms is None:
        until_ms = _optional_int(block.get("reset_at_ms"))
    if until_ms is None:
        until_ms = _optional_int(block.get("retry_at_ms"))
    return BlockStatus(scope=_optional_text(block.get("scope")), until_ms=until_ms)


def _account_state(meta: omp.CredentialMeta | None) -> str:
    """Summarize persisted credential metadata without revealing material."""

    if meta is None:
        return "missing"
    if meta.disabled:
        return "disabled"
    if meta.blocks:
        return "cooling"
    return "ready"


@omp.device("accounts", family="multi-account", rev=1)
async def accounts(
    args: AccountsArgs, ctx: omp.Context
) -> AccountsPayload | AccountsFault:
    """List configured rotation order from secret-free durable block receipts."""

    try:
        configured = _account_order(ctx.settings.get("accounts", ""))
    except ValueError as error:
        return AccountsFault(str(error))

    providers = tuple(dict.fromkeys(account.provider for account in configured))
    if args.provider is not None:
        if args.provider not in providers:
            return AccountsFault(f"provider {args.provider!r} is not configured")
        configured = tuple(account for account in configured if account.provider == args.provider)
        providers = (args.provider,)

    listed = await asyncio.gather(*(omp.creds.list(provider=provider) for provider in providers))
    metadata = {
        (meta.provider, meta.identity): meta
        for provider_accounts in listed
        for meta in sorted(provider_accounts, key=lambda item: item.id, reverse=True)
        if meta.identity is not None
    }
    rows = []
    for position, account in enumerate(configured, start=1):
        meta = metadata.get((account.provider, account.identity))
        successor = _next_scope(account, configured)
        rows.append(
            AccountStatus(
                position=position,
                provider=account.provider,
                identity=account.identity,
                next_identity=successor.identity if successor is not None else None,
                state=_account_state(meta),
                credential_id=meta.id if meta is not None else None,
                expires_at_ms=meta.expires_at_ms if meta is not None else None,
                blocks=(
                    tuple(_block_status(block) for block in meta.blocks)
                    if meta is not None
                    else ()
                ),
            )
        )
    return AccountsPayload(tuple(rows))
