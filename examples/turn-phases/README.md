# Turn phases

## What the pi original did

`@yusukeshib/pi-working-status` replaced pi's plain `Working...` loader with a live line describing what the agent was doing throughout a turn.

## The omp shape

This port folds typed OBSERVE payloads into one immutable, bounded `StatusFacts` snapshot. Turn and message events admit and settle the model request; call events track executor start, progress, and settlement; `capability_budget` reports degradation; `TurnStartEvent.attempt` reports retries; and `compaction_done` reports the typed durable compaction outcome. No phase is guessed from assistant or tool text. A BLAKE2s key suppresses byte-equivalent working-message effects, and submission end restores omp's built-in banner through `ui.set_working_message(None)` ([05-hooks.md §3.11 B-D, G-H](../../docs/py/05-hooks.md#311-the-event-catalog), [07-ui.md §4.8](../../docs/py/07-ui.md#48-transient-chrome)).

The old per-token loader callbacks and text heuristics are deleted. Every STREAM-class subscription (`message_start`, `message_update`, `message_end`, `call_open`, and `tool_update`) declares the same non-zero 50 ms coalesce window in Python and in `omp.toml`; no delta text enters the fold ([05-hooks.md §2.3-2.4](../../docs/py/05-hooks.md#23-where-hooks-attach--and-who-decides), [05-hooks.md `@omp.hook`](../../docs/py/05-hooks.md#31-omphook)). Counters saturate, the debug phase history retains at most 32 transitions, and tool progress that does not change a fact emits no repaint.

## Gaps

None.
