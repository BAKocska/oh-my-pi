from __future__ import annotations

import json
from collections.abc import Awaitable, Callable, Mapping
from dataclasses import dataclass
from typing import Any, Literal

import omp
from omp.agents import AfterIdle, ScheduleScope, Spawn, SubagentSpec

_SCOPE = omp.StateScope.USER
_PROVIDER = "settings-sync"
_CONFLICT_NOTE = (
    "Both local and remote settings changed from their common base; "
    "the local CAS snapshot was retained."
)


@dataclass(frozen=True, slots=True)
class SettingsSyncPushArgs:
    """Arguments for an explicit settings snapshot push."""

    pass


@dataclass(frozen=True, slots=True)
class SettingsSyncPushResult:
    """References written by a successful remote snapshot push."""

    local_ref: str
    base_ref: str
    remote_revision: str | None


@omp.entry_kind("examples.settings_sync.snapshot", rev="v.1")
@dataclass(frozen=True, slots=True)
class SettingsSnapshot:
    """Record the USER-scope current snapshot and its three-way merge inputs."""

    current_ref: str
    base_ref: str
    remote_ref: str
    remote_revision: str | None
    status: Literal["pulled", "synced", "conflict"]


@omp.entry_kind("examples.settings_sync.conflict", rev="v.1", display=True)
@dataclass(frozen=True, slots=True)
class SettingsConflict:
    """Describe a three-way settings conflict without overwriting either side."""

    base_ref: str
    local_ref: str
    remote_ref: str
    note: str


@omp.entry_kind("examples.settings_sync.deferred", rev="v.1")
@dataclass(frozen=True, slots=True)
class SyncDeferred:
    """Record a sync attempt blocked by a missing frozen transport arm."""

    operation: str
    reason: str


@dataclass(frozen=True, slots=True)
class _RemoteSnapshot:
    base: bytes
    settings: bytes
    revision: str | None


@dataclass(frozen=True, slots=True)
class _Resolution:
    selected: bytes
    status: Literal["pulled", "synced", "conflict"]


def _canonical(value: object) -> bytes:
    """Encode one JSON settings object deterministically."""

    if not isinstance(value, Mapping):
        raise ValueError("a settings bundle must be a JSON object")
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def _configured_bundle(ctx: omp.Context) -> bytes:
    """Read the selected settings bundle from extension configuration."""

    raw = ctx.settings.get("bundle", "{}")
    if isinstance(raw, str):
        raw = json.loads(raw)
    return _canonical(raw)


def _reconcile(base: bytes, local: bytes, remote: bytes) -> _Resolution:
    """Resolve a three-way snapshot comparison without silent overwrite."""

    local_changed = local != base
    remote_changed = remote != base
    if local_changed and remote_changed and local != remote:
        return _Resolution(local, "conflict")
    if remote_changed:
        return _Resolution(remote, "pulled")
    return _Resolution(local, "synced")


def _decode_remote(body: bytes) -> _RemoteSnapshot:
    """Decode the conflict-safe remote snapshot envelope."""

    payload = json.loads(body)
    if not isinstance(payload, Mapping):
        raise ValueError("remote settings snapshot must be a JSON object")
    revision = payload.get("revision")
    if revision is not None and not isinstance(revision, str):
        raise ValueError("remote revision must be a string or null")
    return _RemoteSnapshot(
        base=_canonical(payload.get("base", {})),
        settings=_canonical(payload.get("settings", {})),
        revision=revision,
    )


async def _record_resolution(
    remote: _RemoteSnapshot,
    local: bytes,
    *,
    cas_put: Callable[[bytes], Awaitable[object]],
    state_append: Callable[[object], Awaitable[object]],
    journal_append: Callable[[object], object],
) -> _Resolution:
    """Persist one reconciliation, journaling the conflict branch."""

    resolution = _reconcile(remote.base, local, remote.settings)
    base_ref, local_ref, remote_ref = (
        str(await cas_put(remote.base)),
        str(await cas_put(local)),
        str(await cas_put(remote.settings)),
    )
    current_ref = remote_ref if resolution.status == "pulled" else local_ref
    await state_append(
        SettingsSnapshot(
            current_ref=current_ref,
            base_ref=base_ref,
            remote_ref=remote_ref,
            remote_revision=remote.revision,
            status=resolution.status,
        )
    )
    if resolution.status == "conflict":
        journal_append(
            SettingsConflict(
                base_ref=base_ref,
                local_ref=local_ref,
                remote_ref=remote_ref,
                note=_CONFLICT_NOTE,
            )
        )
    return resolution


async def _append_snapshot(entry: object) -> object:
    """Append a snapshot idempotently from its content-addressed identity."""

    assert isinstance(entry, SettingsSnapshot)
    key = (
        f"settings-sync-snapshot:{entry.current_ref}:"
        f"{entry.base_ref}:{entry.remote_ref}"
    )
    return await omp.state.append(entry, scope=_SCOPE, idempotency_key=key)


def _append_conflict(entry: object) -> object:
    """Journal a conflict once for one three-way reference tuple."""

    assert isinstance(entry, SettingsConflict)
    key = (
        f"settings-sync-conflict:{entry.base_ref}:"
        f"{entry.local_ref}:{entry.remote_ref}"
    )
    return omp.journal.append(entry, idempotency_key=key)


async def _credential_headers() -> Mapping[str, str]:
    """Mint a short-lived credential scoped to settings synchronization."""

    token = await omp.creds.mint_scoped("settings", provider=_PROVIDER)
    return {"authorization": f"Bearer {token.token}"}


async def _pull(ctx: omp.Context) -> _Resolution:
    """Pull and reconcile the configured remote snapshot."""

    url = str(ctx.settings.get("remote_url", "")).strip()
    if not url:
        raise ValueError("settings.remote_url must be configured")
    response = await omp.env.http_get(
        url,
        timeout=omp.Duration("10s"),
        headers=await _credential_headers(),
    )
    if response.status != 200:
        raise RuntimeError(f"settings pull failed with HTTP {response.status}")
    remote = _decode_remote(response.body)
    local = _configured_bundle(ctx)
    return await _record_resolution(
        remote,
        local,
        cas_put=lambda data: omp.state.cas_put(data, scope=_SCOPE),
        state_append=_append_snapshot,
        journal_append=_append_conflict,
    )


async def _http_put(url: str, body: bytes, headers: Mapping[str, str]) -> Any:
    """Use the reserved Environment PUT arm when the frozen layer provides it."""

    put = getattr(omp.env, "http_put", None)
    if put is None:
        raise omp.NotWiredError("omp.env.http_put")
    return await put(
        url,
        body=body,
        timeout=omp.Duration("10s"),
        headers=headers,
    )


@omp.device("settings_sync_push", family="settings-sync", rev=1, place="host")
async def settings_sync_push(
    args: SettingsSyncPushArgs, ctx: omp.Context
) -> SettingsSyncPushResult:
    """Push the local bundle only when a three-way comparison is conflict-free."""

    del args
    url = str(ctx.settings.get("remote_url", "")).strip()
    if not url:
        raise ValueError("settings.remote_url must be configured")
    headers = await _credential_headers()
    response = await omp.env.http_get(
        url, timeout=omp.Duration("10s"), headers=headers
    )
    if response.status != 200:
        raise RuntimeError(f"settings pre-push pull failed with HTTP {response.status}")
    remote = _decode_remote(response.body)
    local = _configured_bundle(ctx)
    resolution = await _record_resolution(
        remote,
        local,
        cas_put=lambda data: omp.state.cas_put(data, scope=_SCOPE),
        state_append=_append_snapshot,
        journal_append=_append_conflict,
    )
    if resolution.status == "conflict":
        raise RuntimeError(_CONFLICT_NOTE)

    local_ref = str(await omp.state.cas_put(local, scope=_SCOPE))
    base_ref = str(await omp.state.cas_put(remote.settings, scope=_SCOPE))
    body = json.dumps(
        {
            "base": json.loads(remote.settings),
            "settings": json.loads(local),
            "previous_revision": remote.revision,
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    pushed = await _http_put(url, body, headers)
    if pushed.status not in (200, 201, 204):
        raise RuntimeError(f"settings push failed with HTTP {pushed.status}")
    return SettingsSyncPushResult(local_ref, base_ref, pushed.headers.get("etag"))


@omp.hook("extension_activate")
async def activate(event: omp.ExtensionActivateEvent, ctx: omp.Context) -> None:
    """Pull once per activation generation and arm the durable idle push."""

    try:
        await _pull(ctx)
    except omp.NotWiredError as error:
        omp.journal.append(
            SyncDeferred(
                operation="omp.env.http_get",
                reason=str(error),
            ),
            idempotency_key=f"settings-sync-pull:{event.generation}",
        )

    await omp.agents.schedule(
        "settings-sync-after-idle",
        AfterIdle(omp.Duration(str(ctx.settings.get("quiet_period", "5m")))),
        Spawn(
            SubagentSpec(
                task=(
                    "Push the settings snapshot now by running "
                    "`xd settings_sync_push` in the shell."
                ),
                name="SettingsSyncPush",
                allowed_devices=frozenset({"settings_sync_push"}),
                background=True,
                request_budget=1,
            )
        ),
        scope=ScheduleScope.SESSION,
        overlap="skip",
    )
