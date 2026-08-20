# What the pi original did

`@nklisch/pi-legible` sent assistant prose to a second configurable model and replaced the displayed message with a clearer rewrite. Its settings selected the model and supplied rewriting rules.

# The omp shape

This port keeps the journal authoritative: pi rewrote the message; omp re-renders it. The `assistant` message fold never awaits, writes, launches work, or reads a file. It hashes the host-owned original UTF-8 text and returns the cached rewrite as TML; while the digest is absent, after a failed attempt, or while `/legible` has disabled presentation, it returns `None` so the built-in renderer draws the original. The durable message and its model-facing parts are never amended (`docs/py/07-ui.md` §4.13; `docs/py/08-context.md` §`omp.MessageRef`).

A `turn_end` OBSERVE hook pulls the newly committed assistant `MessageRef.parts()` and calls `omp.agents.completion` once per SHA-256 message digest, outside the synchronous 50 ms fold (`docs/py/05-hooks.md` §4.1; `docs/py/12-agents.md` §“One-shot completions”). Both successful rewrites and failed attempts are atomically cached beneath `await omp.state_dir()`; activation loads those small derived records into memory before folds use them (`docs/py/08-context.md` §“Where the store lives”). `[settings].role` selects the completion role and `[settings].rules` supplies its legibility policy. `/legible` appends a typed `LegibleToggle` to `omp.StateScope.SESSION`, so toggling changes presentation only and survives replay (`docs/py/09-journal.md` §`omp.StateScope`).

Deleted mechanisms: message replacement, transcript mutation, inference inside rendering, mutable-message aliases, renderer-owned timers, and re-registration on toggle. The manifest's tool/declaration array is byte-identical across toggles; only SESSION state changes availability of the rewritten view.

# Gaps

- `omp.MessageView` is missing: `docs/py/07-ui.md` §4.13 lines 1481–1508 specifies `def f(message: MessageView, ctx: RenderCtx)` and says `docs/py/08-context.md` owns the read-only projection, but that document defines only `omp.MessageRef`; the frozen dispatcher accepts an untyped `object` at `crates/py/python/omp/ui/__init__.py:708-720`, and no frozen module exports `MessageView`. The example's private `text` protocol is therefore the narrow assumption needed to demonstrate the port, not a frozen contract.
- `omp.message_renderer` cannot satisfy both documented purity and pending-cache replacement: `docs/py/07-ui.md` lines 1467–1471 requires a fold to be deterministic in `(state, ctx)`, while §4.13 lines 1481–1485 passes only the immutable message view and render context. An off-fold rewrite becoming available necessarily changes the result for the same pair, and the frozen layer exposes neither an immutable presentation-cache field nor a transcript invalidation effect (`crates/py/python/omp/ui/__init__.py:666-720`). This example keeps all I/O outside the fold and uses a memory snapshot, but the snapshot transition remains outside the documented pure-fold contract.
