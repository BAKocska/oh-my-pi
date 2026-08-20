# Fuzzy index

## What the pi original did

`@ff-labs/pi-fff` replaced the built-in find and grep tools with native FFF-backed fuzzy search, maintained private SQLite/LMDB history and frecency state, ran its own directory scanner and thread pool, and supplied per-keystroke `@`-mention completion (`catalog.md:158`; `docs/py/01-devices.md` § “`@ff-labs/pi-fff` → precedence over `grep`”).

## The omp shape

A warm `fuzzy-index` worker lives at the Environment site with `idle_ttl = "0s"`. Its boot function streams `omp.env.find.walk()`, inheriting the shared workspace and ignore semantics instead of maintaining a private walker, then reads text through document leases and keeps the rebuildable path/line index only in worker memory. It writes no side database or cache-as-truth. `ffind` and `fgrep` return typed ranked matches; an encoded result above `omp.workers.RESULT_SPILL_BYTES` becomes `omp.Spill`, so no temporary path crosses the boundary. Calls use the sole `dyn` dispatch tree through `invoke/ffind` and `invoke/fgrep`.

The intended catalog policy is a sub-CORE claim: core retains its model slot, the lower implementation is reachable at a claimant-qualified path and is never separately advertised (`docs/py/01-devices.md` § “Namespacing and ordered precedence”, especially lines 384–423). The frozen decorator cannot express that claim, so this example follows the requested fallback and declares `ffind` and `fgrep` as ordinary soft devices rather than pretending to replace core tools. This is the warm-worker pattern from `docs/py/04-placement.md` §2, with the native addon, private persistence, private scan thread pool, flags, and synchronous editor autocomplete deleted.

## Gaps

- **Sub-CORE claims are absent from the frozen API.** `omp.Precedence` and the documented `replaces=` / `precedence=` parameters of `omp.device` are missing from `crates/py/python/omp/__init__.py` (`device`, lines 361–368), despite `docs/py/01-devices.md` § “Namespacing and ordered precedence” and § “`@ff-labs/pi-fff` → precedence over `grep`”. Consequently this port uses the explicitly allowed plain-soft-device fallback and cannot demonstrate shadowed claimant-qualified dispatch.
- **A rescan cannot trigger a supported rebuild.** No `WATCH_RESCANNED` event exists in `crates/py/python/omp/events.py`, and `_Workers.restart` is absent from `crates/py/python/omp/placement.py`, despite `docs/py/11-env.md` open question 4 (“Document events and the walker cache”) and `docs/py/04-placement.md` § `omp.workers` documenting `await omp.workers.restart(...)`. The index therefore rebuilds on worker generation boot only; workspace-wide watch coherence remains the stated open Q4.
