# Goal loop

## What the pi original did

`@narumitw/pi-goal` registered an objective, watched turn lifecycle events, and injected follow-up messages until it considered the goal complete. It implemented private progress and repetition bookkeeping that could race other continuation extensions.

## The omp shape

The soft `goal` device keeps `goal_set`, `goal_status`, and `goal_complete` plus typed session snapshots. `goal_set` starts the session-scoped `goal-loop` regime with journaled `GoalState`; `goal_complete` records completion and stops that regime.

At `SETTLE`, unmet work stages context and selects retry:

```python
def goal_loop(ctx, next_):
    state = ctx.state.value
    if state.complete:
        return next_.complete()

    ctx.context.append(omp.user_text(f"Continue toward: {state.objective}"))
    return next_.retry()
```

A transient stall may return normally without ending the active regime. Core's continuation ledger remains the global backstop rather than regime-private retry bookkeeping.

See [`docs/py/15-regimes.md`](../../docs/py/15-regimes.md) for fixed events, transactional effects, and durable state.
