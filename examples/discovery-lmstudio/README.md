## What the pi original did

`pi-lmstudio` connected Pi to local LM Studio servers, queried their model catalogs on every turn, registered every model it found, and unregistered providers that appeared offline. Its origin is cataloged in `.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md` as dynamically reconciling LM Studio models per turn.

## The omp shape

This is the class (a) + (b) worked port from `docs/py/13-inference.md` §2. `@omp.provider` declares the local OpenAI-compatible route once, while `DiscoverySpec(interval=omp.Duration("30s"))` schedules a daemon-deduplicated background poll above the 5-second floor. The sole parser maps LM Studio's specialized discovery shape into catalog `ModelSpec` rows; there is no register/unregister churn and no per-turn polling code.

A successful authoritative page makes absence evidence: models omitted by that poll retire. A non-authoritative page preserves previously known rows, and a raised poll retains the last successful rows. This kills the original dropped-packet-deletes-your-models bug: transport failure is no longer misrepresented as authoritative absence. The hook uses the Environment's loopback namespace and the route declares `TrustDomain.loopback()`, following `docs/py/13-inference.md` §`models_discover` and §2.

## Gaps

- `DiscoverySpec`, `DiscoveryKind`, and `TrustDomain` are documented by `docs/py/13-inference.md` §`DiscoverySpec and DiscoveryDefaults` and used by the §2 worked port, but are absent from `crates/py/python/omp/provider.py`; `RouteSpec.discovery` and `.trust` remain `object | None` at lines 395-406 and none of these symbols appears in `provider.py`'s export list at lines 1058-1071.
- `DiscoveryQuery` and `DiscoveryPage` exist in `crates/py/python/omp/provider.py:944-970`, but the top-level exports documented and used by `docs/py/13-inference.md` §2 are absent from `crates/py/python/omp/__init__.py:442-493` and `:765-823`. This example imports them from their frozen owner module.
- `omp.env.http_get`, required by `docs/py/13-inference.md` §`models_discover` and the §2 worked port, is absent from the frozen Environment surface at `crates/py/python/omp/env.py:936-997`; the same document's implementation-gap table explicitly records the missing frame at `docs/py/13-inference.md:1932-1934`.
- The §2 worked port declares `@omp.hook("models_discover", provider="lmstudio")` without a phase at `docs/py/13-inference.md:1718-1719`, but frozen `omp.hook` rejects every otherwise-gateable event without an explicit phase at `crates/py/python/omp/hooks.py:343-345`. This example supplies `HookPhase.TRANSFORM` explicitly.
