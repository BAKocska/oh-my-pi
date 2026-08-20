## What the pi original did

`pi-pulse` attached stopwatches to `before_provider_request`, `message_start`, every streamed text/reasoning/tool-argument delta, and `message_end`. It estimated token-like deltas, refreshed a custom footer on a timer, persisted rolling samples, and also ticked a presentation-only wall clock.

## The omp shape

This port deletes the per-token stopwatch, token estimator, stream hooks, ticker, persistence snapshot, clock, and custom footer factory. One bounded `@omp.telemetry` subscription receives only settled `model_request` records, coalesces queued updates by served model, and derives decode TPS, TTFT, and end-to-end response time from typed event data; the Python metric path never reads a clock. A rebuildable in-memory EMA is kept per served model. `DropStats.dropped` and `DropStats.coalesced` measure holes in each delivery window, and a lossy window marks the latest response with the semantic `err` theme token. The retained `Slot.FOOTER` contribution renders only `tps · ttft · last`; the host owns layout, coalescing, theme resolution, and terminal rendering (`docs/py/10-telemetry.md` §2, §`ModelRequest`, and §`DropStats`; `docs/py/07-ui.md` §§2.9, 3, and 4).

## Gaps

None — every symbol this port needs is frozen.
