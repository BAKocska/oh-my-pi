# Plan mode

## What the pi original did

`@dreki-gg/pi-plan-mode` combined a planning model and toolset takeover, a standing write guard, and three settlement nudges. It also re-registered tools, rewrote stale context, and hosted a loopback plan viewer.

## The omp shape

Invoke this extension's soft `plan` tool with `op="on"`, `op="status"`, or `op="off"`. The app's built-in `/plan` command routes to Core's `plan` regime, so it is not this extension's entry point.

`op="on"` starts two regimes:

1. `plan-mode` is session-scoped, owns `mode` and `worktree`, and sets scoped `toolset`, `model`, and `prompt` values. Owning `mode` makes it a visible mode; there is no separate mode runtime.
2. `plan-settle-gate` is a bounded companion regime subscribed to `SETTLE`. It may retry up to three committed nudges before completing quietly.

At `ADMISSION`, writes select rejection:

```python
def plan(ctx, next_):
    if ctx.event.is_write:
        return next_.reject("plan mode is read-only")
```

At `SETTLE`, the companion stages its instruction through `ctx.context.append(...)` and returns `next_.retry()`. `op="off"` records the typed plan and stops the companion before stopping the mode; mode stop atomically releases its resource leases and restores scoped settings.

Client-side context rewriting and the loopback viewer remain deleted.

See [`docs/py/15-regimes.md`](../../docs/py/15-regimes.md) for modes, bounds, and atomic resource release.
