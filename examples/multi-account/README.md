# Multi-account rotation

## What the pi original did

[`pi-multi-account`](https://github.com/Sarrius/pi-multi-account) discovered credentials by reading `~/.pi/agent/auth.json`, registered per-account provider aliases, parsed quota errors, and maintained cooldowns, invalidations, recent switches, and pending resume work in `provider-failover-state.json`. It also wrote a separate decision log and hashed credential material to deduplicate accounts.

## The omp shape

`[settings.accounts]` declares an ordered set of non-secret `provider:identity` scopes. The `provider_error` hook uses the frozen `ErrorKind` classification and returns `Failover.rotate_account(cooldown=...)` only for uncommitted `RATE_LIMITED` and `QUOTA_EXHAUSTED` failures. Core owns selection and persists the cooldown for the current `(provider, route, identity)`; there is no extension state file, timer, error regex, retry loop, or continuation prompt (`docs/py/13-inference.md`, **provider_error**, lines 1428–1489).

The soft `accounts` tool calls `omp.creds.list()` for each manifest-allowed provider and exposes only `CredentialMeta`: identity, numeric credential id, expiry, disabled state, and projected block scope/expiry. Its `ready`/`cooling`/`disabled`/`missing` status therefore comes from host-owned durable block receipts, not a telemetry listener or extension log. The extension never calls `omp.creds.reveal()`, never formats `ProviderError.message`, and never places `Secret` material in a payload, journal entry, notification, or log (`docs/py/13-inference.md`, **Credentials: scoped, and secret-free by default**, lines 205–228, and **omp.creds**, lines 1547–1575).

The original's `auth.json` harvesting, token fingerprinting, provider-alias credential copies, and hand-written auth/state/debug files are deliberately deleted. Credentials remain in Core's encrypted store and reach Python only through the scoped `omp.creds` facade or an explicitly typed `Secret` arm; this example needs neither import nor reveal authority.

## Gaps

- `Failover.rotate_account(*, cooldown=...)` cannot name the configured successor identity (or a successor provider), so the hook can verify the declared next scope but Core ultimately chooses an eligible same-provider account by its own pool ordering rather than `[settings.accounts]` order. Frozen surface: `crates/py/python/omp/provider.py:999-1002`; documented surface: `docs/py/13-inference.md`, **provider_error**, lines 1465–1489. A target identity on `Failover.rotate_account` (validated against the manifest-scoped credential pool) is required for exact declared-order rotation.
