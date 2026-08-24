# Task queue

## What the pi original did

`pi-true-queue` held later tasks out of the agent's context until the current task finished, then released exactly one queued task.

## The omp shape

The `queue_task` device and `/queue_task` command append typed `QueuedTask` transitions to the session journal. They start the session-scoped `task-queue-drain` regime only when it is absent. Waiting task text occupies no prompt slot, message, or other context item.

At `SETTLE`, the regime folds the journal. Its typed state remembers the task handed out by the last committed handler, so only a later settle can mark that task done. Releasing the oldest waiting task is one transaction:

```python
def drain(ctx, next_):
    task = oldest_waiting(ctx.view)
    ctx.context.append(task)
    ctx.state.replace(ctx.state.value.with_active(task.id))
    return next_.retry()
```

A drained queue returns `next_.complete()`. `max_steps=8` provides a backstop, and Core advances the bound only after the hand-out effect commits. Journal replay reconstructs queue order after process replacement.

Unlike `goal-loop`, which repeatedly evaluates one objective, this regime serializes independent entries and runs each exactly once.

See [`docs/py/15-regimes.md`](../../docs/py/15-regimes.md) for activation, bounds, and delivery-aware accounting.
