"""Manifest-scoped credential CONTROL requests.

The host restricts every operation to providers named by the extension's
``credentials.allow`` declaration. Secret disclosure through :func:`reveal`
additionally requires the ``credentials.reveal`` grant and is journaled by the
host; ordinary operations expose metadata or short-lived scoped tokens only.
"""

from __future__ import annotations

import base64
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from decimal import Decimal
from types import MappingProxyType
from typing import Any

from _omp import Duration, Secret

from . import _control_backend, _control_request
from ._errors import NotWiredError
from .provider import (
    Credential,
    CredentialKind,
    UsageReport,
    UsageScope,
    UsageUnit,
    UsageWindow,
)

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


def _sealed(secret: Secret) -> dict[str, str]:
    if not isinstance(secret, Secret):
        raise TypeError("credential secret material must be an omp.Secret")
    with secret.use() as exposed:
        encoded = base64.b64encode(exposed).decode("ascii")
    return {"encoding": "base64", "data": encoded}


def _unseal(value: object) -> Secret:
    if not isinstance(value, Mapping):
        raise TypeError("credential reveal response must be a sealed mapping")
    if value.get("encoding") != "base64" or not isinstance(value.get("data"), str):
        raise TypeError("credential reveal response has an invalid sealed encoding")
    try:
        raw = base64.b64decode(value["data"], validate=True)
    except (ValueError, TypeError) as error:
        raise TypeError("credential reveal response has invalid base64") from error
    return Secret(raw)


def _wire_credential(cred: Credential) -> dict[str, object]:
    if not isinstance(cred, Credential):
        raise TypeError("store expects an omp.Credential")
    return {
        "kind": cred.kind.value,
        "secret": _sealed(cred.secret),
        "refresh_token": (
            None if cred.refresh_token is None else _sealed(cred.refresh_token)
        ),
        "expires_at_ms": cred.expires_at_ms,
        "identity": cred.identity,
        "props": dict(cred.props),
    }


def _decode_meta(value: object) -> CredentialMeta:
    if isinstance(value, CredentialMeta):
        return value
    if not isinstance(value, Mapping):
        raise TypeError("credential metadata response must be a mapping")
    try:
        raw_blocks = value.get("blocks", ())
        if not isinstance(raw_blocks, Sequence) or isinstance(
            raw_blocks, (str, bytes, bytearray)
        ):
            raise TypeError("credential blocks must be a sequence")
        blocks = tuple(
            MappingProxyType(dict(block))
            for block in raw_blocks
            if isinstance(block, Mapping)
        )
        if len(blocks) != len(raw_blocks):
            raise TypeError("credential block must be a mapping")
        state = str(value.get("state", "")).lower()
        return CredentialMeta(
            id=int(value["id"]),
            provider=str(value["provider"]),
            identity=None if value.get("identity") in (None, "") else str(value["identity"]),
            kind=CredentialKind(str(value["kind"]).lower()),
            expires_at_ms=(
                None
                if value.get("expires_at_ms") in (None, 0)
                else int(value["expires_at_ms"])
            ),
            disabled=bool(value.get("disabled", state == "disabled")),
            disabled_cause=(
                None
                if value.get("disabled_cause") in (None, "")
                else str(value["disabled_cause"])
            ),
            blocks=blocks,
        )
    except (KeyError, TypeError, ValueError) as error:
        raise TypeError("credential metadata response is malformed") from error


def _decode_usage_window(value: object) -> UsageWindow:
    if isinstance(value, UsageWindow):
        return value
    if not isinstance(value, Mapping):
        raise TypeError("credential usage window must be a mapping")
    fraction = value.get("fraction")
    return UsageWindow(
        id=str(value["id"]),
        used=None if value.get("used") is None else int(value["used"]),
        limit=None if value.get("limit") is None else int(value["limit"]),
        fraction=None if fraction is None else Decimal(str(fraction)),
        resets_at_ms=(
            None if value.get("resets_at_ms") is None else int(value["resets_at_ms"])
        ),
        unit=UsageUnit(str(value.get("unit", UsageUnit.REQUESTS.value)).lower()),
    )


def _decode_usage(value: object) -> UsageReport | None:
    if value is None or isinstance(value, UsageReport):
        return value
    if not isinstance(value, Mapping):
        raise TypeError("credential usage response must be a mapping")
    rows = value.get("windows", ())
    if not isinstance(rows, Sequence) or isinstance(rows, (str, bytes, bytearray)):
        raise TypeError("credential usage windows must be a sequence")
    try:
        return UsageReport(
            windows=tuple(_decode_usage_window(row) for row in rows),
            balance_nanos_usd=(
                None
                if value.get("balance_nanos_usd") is None
                else int(value["balance_nanos_usd"])
            ),
            plan=None if value.get("plan") is None else str(value["plan"]),
            observed_at_ms=(
                None
                if value.get("observed_at_ms") is None
                else int(value["observed_at_ms"])
            ),
        )
    except (KeyError, TypeError, ValueError) as error:
        raise TypeError("credential usage response is malformed") from error


async def _request(method: str, /, **arguments: Any) -> Any:
    operation = f"omp.creds.{method}"
    if _control_backend.get() is None:
        raise NotWiredError(operation)
    return await _control_request(operation, **arguments)


async def list(provider: str | None = None) -> tuple[CredentialMeta, ...]:
    """Return secret-free metadata for stored credentials."""
    rows = await _request("list", provider=provider)
    if not isinstance(rows, Sequence) or isinstance(rows, (str, bytes, bytearray)):
        raise TypeError("credential list response must be a sequence")
    return tuple(_decode_meta(row) for row in rows)


async def store(cred: Credential, *, provider: str | None = None) -> CredentialMeta:
    """Atomically persist a credential and return its metadata."""
    return _decode_meta(
        await _request("store", cred=_wire_credential(cred), provider=provider)
    )


async def refresh(*, id: int | None = None, provider: str | None = None) -> CredentialMeta:
    """Refresh one credential through the host's single-flight lease."""
    return _decode_meta(await _request("refresh", id=id, provider=provider))


async def clear(*, id: int | None = None, provider: str | None = None) -> None:
    """Delete the selected stored credential."""
    await _request("clear", id=id, provider=provider)


async def disable(id: int, cause: str) -> CredentialMeta:
    """Disable a credential without deleting it."""
    return _decode_meta(await _request("disable", id=id, cause=cause))


async def enable(id: int) -> CredentialMeta:
    """Re-enable a disabled credential."""
    return _decode_meta(await _request("enable", id=id))


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
    if not isinstance(scope, UsageScope):
        raise TypeError("scope must be an omp.UsageScope")
    return _decode_usage(
        await _request(
            "usage",
            scope=scope.value,
            allow_stale=allow_stale,
            provider=provider,
        )
    )


async def mint_scoped(
    facet: str,
    *,
    ttl: Duration | None = None,
    provider: str | None = None,
) -> ScopedToken:
    """Mint a short-lived token restricted to a provider-defined facet."""
    value = await _request(
        "mint_scoped",
        facet=facet,
        ttl=None if ttl is None else str(ttl),
        provider=provider,
    )
    if isinstance(value, ScopedToken):
        return value
    if not isinstance(value, Mapping):
        raise TypeError("scoped-token response must be a mapping")
    try:
        return ScopedToken(
            token=str(value["token"]),
            expires_at_ms=int(value["expires_at_ms"]),
        )
    except (KeyError, TypeError, ValueError) as error:
        raise TypeError("scoped-token response is malformed") from error


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

    return _decode_meta(
        await _request(
            "import_oauth",
            refresh_token=_sealed(refresh_token),
            access_token=None if access_token is None else _sealed(access_token),
            expires_at_ms=expires_at_ms,
            identity=identity,
            props=dict(props),
            provider=provider,
        )
    )


async def reveal(*, id: int | None = None, provider: str | None = None) -> Secret:
    """Reveal a credential through the separately granted, audited host arm."""
    return _unseal(await _request("reveal", id=id, provider=provider))


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
