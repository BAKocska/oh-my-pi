# Durable regime retry

This extension declares a session-scoped `@omp.regime` with typed `RetryState`. It retries two settled turns, then requires the `write` tool on the third.
The reminder and retry control are staged at `SETTLE`; the exclusive tool request is staged only at the fixed `TOOL_CHOICE` event.

Core seals the declaration during FREEZE. Its state envelope is `regime_retry.RetryState@1`, so replacing the extension-host process restores the same step and turn count from the journal. An incompatible revision fails the active regime instead of loading stale state.

The handler stages effects on `ctx` and selects control through `next_`:

```python
def three_turn_retry(ctx, next_):
    if ctx.event.point is omp.TOOL_CHOICE:
        if ctx.state.value.turns >= 2:
            ctx.tool.require("write")
        return

    state = RetryState(ctx.state.value.turns + 1)
    ctx.state.replace(state)
    if state.turns < 3:
        return next_.retry()

    return next_.complete()
```

Start it with:

```python
await omp.regimes.start("three-turn-retry", state=RetryState())
```

A simultaneous core tool requirement is serialized through the `tool_choice` resource queue. A queued requirement does not consume a bound step until its effect commits.

See [`docs/py/15-regimes.md`](../../docs/py/15-regimes.md) for middleware isolation and durability.
