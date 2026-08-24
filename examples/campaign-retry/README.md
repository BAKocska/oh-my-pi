# Durable regime retry

This extension declares a session-scoped `@omp.regime` with typed `RetryState`. It retries two settled turns, then requires the `write` tool on the third.

Core seals the declaration during FREEZE. Its state envelope is `examples.regime-retry.state@1`, so replacing the extension-host process restores the same step and turn count from the journal. An incompatible revision fails the active regime instead of loading stale state.

The handler stages effects on `ctx` and selects control through `next_`:

```python
def three_turn_retry(ctx, next_):
    if ctx.state.value.turn < 3:
        ctx.state.replace(ctx.state.value.incremented())
        return next_.retry()

    ctx.tool.require("write")
    return next_.complete()
```

Start it with:

```python
await omp.regimes.start("three-turn-retry", state=RetryState())
```

A simultaneous core tool requirement is serialized through the `tool_choice` resource queue. A queued requirement does not consume a bound step until its effect commits.

See [`docs/py/15-regimes.md`](../../docs/py/15-regimes.md) for middleware isolation and durability.
