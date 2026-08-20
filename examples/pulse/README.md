## What the pi original did

`pi-pulse` attached stopwatches to `before_provider_request`, `message_start`, every streamed text/reasoning/tool-argument delta, and `message_end`. It estimated token-like deltas, refreshed a custom footer on a timer, persisted rolling samples, and also ticked a presentation-only wall clock.

## The omp shape

This port deletes the per-token stopwatch, token estimator, stream hooks, ticker, persistence snapshot, clock, and custom footer factory. One bounded `@omp.telemetry` subscription receives only settled `model_request` records, coalesces queued updates by served model, and derives decode TPS, TTFT, and end-to-end response time from typed event data; the Python metric path never reads a clock. A rebuildable in-memory EMA is kept per served model. `DropStats.dropped` and `DropStats.coalesced` measure holes in each delivery window, and a lossy window marks the latest response with the semantic `err` theme token. The retained `Slot.FOOTER` contribution renders only `tps · ttft · last`; the host owns layout, coalescing, theme resolution, and terminal rendering (`docs/py/10-telemetry.md` §2, §`ModelRequest`, and §`DropStats`; `docs/py/07-ui.md` §§2.9, 3, and 4).

## Gaps

- `omp.telemetry.ModelRequest.latency_ms` and `omp.telemetry.ModelRequest.ttft_ms` are absent from the frozen class at `crates/py/python/omp/telemetry.py:125-131`, while `docs/py/10-telemetry.md` §`class ModelRequest(Envelope)` (`:781-800`) requires both typed timing fields. Until the dispatch schema is corrected, the decorated sink cannot receive the data needed for wall time or TTFT.
- `omp.telemetry.ModelRequest.tokens` in `docs/py/10-telemetry.md:796` diverges from the frozen `omp.telemetry.ModelRequest.usage` field at `crates/py/python/omp/telemetry.py:129`. This port uses the real frozen `usage.output` spelling for TPS.
- `omp.telemetry(..., coalesce_key=...)` validates the callable but drops it when registering the subscription: `crates/py/python/omp/telemetry.py:148-183` calls `register_telemetry` without the key, and `crates/py/python/omp/_registry.py:184-195,494-517` has no field or parameter for it. This contradicts `docs/py/10-telemetry.md` §`@omp.telemetry` (`:193-211`), so `Overflow.COALESCE_BY_KEY` cannot be wired from the frozen declaration.
