# Settings sync

## What the pi original did

`@narumitw/pi-sync` synchronized selected Pi settings and, optionally, sessions through Git, WebDAV, R2, or S3-compatible storage using conflict-safe snapshots. It automatically pulled at session start and bounded its shutdown push.

## The omp shape

This worked example intentionally ports only settings. The configured JSON bundle is stored as immutable USER-scope `omp.state` CAS values, with typed scoped-log pointers; no loose files or parallel database exist. Activation performs an idempotent pull and a three-way comparison. If local and remote both differ from their common base, the local snapshot remains authoritative and a typed `SettingsConflict` note is appended to the session journal instead of silently overwriting either side (`docs/py/09-journal.md:523-604`, *`omp.state`*).

A durable `omp.agents.AfterIdle` schedule requests `settings_sync_push` after the configured quiet period. It uses the core scheduler rather than a timer (`docs/py/12-agents.md:831-901`, *Schedules, timers, and durable delivery*). The remote bearer is minted through `omp.creds`, never read from settings (`docs/py/13-inference.md:205-238`, *Credentials: scoped, and secret-free by default*). Pull is attached to `extension_activate`, including restart replay (`docs/py/05-hooks.md:1899-1912`).

Session synchronization is deliberately out of scope: the session journal is already omp's append-only durable truth, and extensions must use `omp.sessions` to read it rather than copy or rewrite session files (`docs/py/09-journal.md:606-613`).

## Gaps

- `omp.env.http_get` exists but unconditionally raises `NotWiredError` (`crates/py/python/omp/env.py:905-913`), so pull-on-activate is durably recorded as deferred on the frozen layer. This is also acknowledged in `docs/py/13-inference.md:1767-1769`.
- `omp.agents.schedule` unconditionally raises `NotWiredError` (`crates/py/python/omp/agents.py:923-936`) despite the durable schedule contract in `docs/py/12-agents.md:831-901`; therefore the declared `AfterIdle` push cannot arm on the frozen layer.
