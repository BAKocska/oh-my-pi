## What the pi original did

`@narumitw/pi-goal` registered an objective, watched turn lifecycle events, and injected follow-up messages until it considered the goal complete. It also implemented its own progress and repetition bookkeeping, which could race other continuation extensions and relied on interpreting message text.

## The omp shape

The port exposes one soft `goal` device with `goal_set`, `goal_status`, and `goal_complete` operations, and stores typed snapshots in `omp.state` at `StateScope.SESSION`. Its only loop seam is the domain-return `agent_settled` hook: an unmet goal returns `Continue(prompt=..., label=..., collapse_prior=True)`, while a completed goal or Core-provided `LoopSignal.stalled` returns `Settle()` (docs/py/05-hooks.md §4.2 and docs/py/12-agents.md, “Autonomous loops”). There is no `turn_end`/`sendMessage` emulation, message-text repeat detector, context-cleanup hook, or local continuation counter; Core's recursive `ContinuationLedger` owns the budget and `collapse_prior` owns marker compaction. A surviving `Continue` remains subject to `defer_interrupts`, so deferred-interrupt sessions defer this hook-driven continuation like every other drain point.

## Gaps

- No frozen-vs-docs signature divergence was encountered for the available `omp.state` and `omp.StateScope` surface.
