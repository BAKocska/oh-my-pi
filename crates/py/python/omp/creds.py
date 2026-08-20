"""Manifest-scoped credential CONTROL requests.

The host restricts every operation to providers named by the extension's
``credentials.allow`` declaration. Secret disclosure through :func:`reveal`
additionally requires the ``credentials.reveal`` grant and is journaled by the
host; ordinary operations expose metadata or short-lived scoped tokens only.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from types import MappingProxyType
from typing import Any

from _omp import Duration, Secret

from . import _control_backend, _control_request
from ._errors import NotWiredError
from .provider import Credential, CredentialKind, UsageReport, UsageScope

_FROZEN_EMPTY: Mapping[str, int | str | bool] = MappingProxyType({})


@dataclass(frozen=True, slots=True)
class CredentialMeta:
    """Describe one stored credential without exposing secret material."""

    id: int
    provider: str
    identity: str | None
    kind: CredentialKind
    expires_at_ms: int | None = None
    disabled: bool = False
    disabled_cause: str | None = None
    blocks: tuple[Mapping[str, object], ...] = ()


@dataclass(frozen=True, slots=True)
class ScopedToken:
    """Carry a short-lived token restricted to one provider-defined facet."""

    token: str
    expires_at_ms: int


async def _request(method: str, /, **arguments: Any) -> Any:
    operation = f"omp.creds.{method}"
    if _control_backend.get() is None:
        raise NotWiredError(operation)
    return await _control_request(operation, **arguments)


async def list(provider: str | None = None) -> tuple[CredentialMeta, ...]:
    """Return secret-free metadata for stored credentials."""
    return await _request("list", provider=provider)


async def store(cred: Credential, *, provider: str | None = None) -> CredentialMeta:
    """Atomically persist a credential and return its metadata."""
    return await _request("store", cred=cred, provider=provider)


async def refresh(*, id: int | None = None, provider: str | None = None) -> CredentialMeta:
    """Refresh one credential through the host's single-flight lease."""
    return await _request("refresh", id=id, provider=provider)


async def clear(*, id: int | None = None, provider: str | None = None) -> None:
    """Delete the selected stored credential."""
    await _request("clear", id=id, provider=provider)


async def disable(id: int, cause: str) -> CredentialMeta:
    """Disable a credential without deleting it."""
    return await _request("disable", id=id, cause=cause)


async def enable(id: int) -> CredentialMeta:
    """Re-enable a disabled credential."""
    return await _request("enable", id=id)


async def report_block(
    *,
    until_ms: int,
    scope: str | None = None,
    id: int | None = None,
    provider: str | None = None,
) -> None:
    """Persist a rate-limit or quota block for a credential scope."""
    await _request(
        "report_block", until_ms=until_ms, scope=scope, id=id, provider=provider
    )


async def usage(
    *,
    scope: UsageScope = UsageScope.ALL,
    allow_stale: bool = True,
    provider: str | None = None,
) -> UsageReport | None:
    """Return the resolved provider usage report, when one is available."""
    return await _request(
        "usage", scope=scope, allow_stale=allow_stale, provider=provider
    )


async def mint_scoped(
    facet: str,
    *,
    ttl: Duration | None = None,
    provider: str | None = None,
) -> ScopedToken:
    """Mint a short-lived token restricted to a provider-defined facet."""
    return await _request("mint_scoped", facet=facet, ttl=ttl, provider=provider)


async def import_oauth(
    *,
    refresh_token: Secret,
    access_token: Secret | None = None,
    expires_at_ms: int | None = None,
    identity: str | None = None,
    props: Mapping[str, int | str | bool] = _FROZEN_EMPTY,
    provider: str | None = None,
) -> CredentialMeta:
    """Import externally obtained OAuth material through the audited host arm."""
    return await _request(
        "import_oauth",
        refresh_token=refresh_token,
        access_token=access_token,
        expires_at_ms=expires_at_ms,
        identity=identity,
        props=props,
        provider=provider,
    )


async def reveal(*, id: int | None = None, provider: str | None = None) -> Secret:
    """Reveal a credential through the separately granted, audited host arm."""
    return await _request("reveal", id=id, provider=provider)


__all__ = (
    "Credential",
    "CredentialKind",
    "CredentialMeta",
    "ScopedToken",
    "Secret",
    "UsageReport",
    "UsageScope",
    "clear",
    "disable",
    "enable",
    "import_oauth",
    "list",
    "mint_scoped",
    "refresh",
    "report_block",
    "reveal",
    "store",
    "usage",
)
