## What the pi original did

`pi-llama-switch` restarted a local `llama-server` with named model-specific flags, including context size, vision projector, and sampling options. The survey catalogs it at `.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:391` as a single-model setup that changes server flags when switching models.

## The omp shape

The named flag sets live entirely in `[settings.configs]`. `/llama <config>` validates one set, marks the local provider unavailable, stops the Environment-owned `examples.llama-switch.server` process tree, and calls `omp.env.proc.ensure` with the new quoted argv. `ReadyAll(ReadyLog(...), ReadyTcp(...))` makes readiness observed rather than a `sleep` guess, and the Environment advances the process generation. State publication rejects an old generation before it can change availability. This follows `docs/py/11-env.md` §“Named processes — omp.env.proc” and `docs/py/00-overview.md` §“Idempotency, duplicate delivery, and generation fencing.”

The provider is declared once as an OpenAI Chat-compatible loopback route with ordinary `/models` discovery. Discovery is explicitly non-authoritative, so a failed or partial listing never retires the last known rows (`docs/py/13-inference.md` §“DiscoverySpec and DiscoveryDefaults”). Process readiness is reflected by atomically replacing the declared model's `ProviderAvailability`; provider declarations are not repeatedly registered.

Deleted from the original shape: `pkill`, ambient subprocess spawning, PID ownership, hand-selected sleeps, ad hoc port probing, and teardown handlers. The Environment owns the process group, readiness probes, retained launch specification, endpoint, and generation.

## Gaps

- `omp.env.Process.restart()` is documented at `docs/py/11-env.md:1149`, but the frozen `Process` class ends with `stop()` at `crates/py/python/omp/env.py:1033-1077`. The same docs say `restart()` reuses the retained launch spec, which cannot express this port's required new argv. This example therefore uses the fully frozen `Process.stop()` → `omp.env.proc.ensure(name, new_script, ...)` path.
- Frozen process-handle requests do not transmit `Process.generation`: `info`, `output`, `states`, `send`, `signal`, and `stop` dispatch by name only at `crates/py/python/omp/env.py:1048-1077`, while `docs/py/11-env.md:1139-1149` describes a stable generation handle and `docs/py/00-overview.md`'s generation-fencing contract requires old-generation frames to be rejected. This example fences every availability transition locally before publishing it.
