## What the pi original did

`pi-true-queue` held later tasks out of the agent's context until the current task finished, then released exactly one queued task. This prevented a running task from being distracted by future work.

## The omp shape

The `queue_task` device and `/queue_task` command append typed `QueuedTask` transitions to the session journal, then imperatively engage the Session-scoped `task-queue-drain` campaign with `queue=True` as specified by docs/py/15-campaigns.md §4.3. The entry kind deliberately has no `project` method, and the command returns an empty `ui.Consumed()` rather than a `ui.Prompt` or notice, so waiting task text occupies no prompt slot, message, or other context item (docs/py/09-journal.md, “Recognized methods on the decorated class”; docs/py/07-ui.md §4.15).

At `omp.SETTLE`, the campaign folds the journal, marks the one started entry done, and emits `omp.Continue(inject=<next task>)` for only the oldest waiting entry. A drained queue emits `omp.Done()`. This is doc-15 §1's takeover skeleton and §6's “Todo completion reminder” porting shape expressed through the frozen verdict vocabulary (§2.2, §4.0-4.5), rather than a domain hook that hand-rolls settlement control. The journal remains the only queue state and therefore reconstructs the same ordering after process replacement. `omp.Ladder(8)` provides the campaign-local finite bound; Core's `ContinuationLedger` remains the global backstop underneath it as described in §2.3.

Unlike `goal-loop`, which repeatedly tests and advances one goal until a completion predicate is met, this extension is a work queue: every entry runs once, in insertion order, under strict serialization.

## Gaps

- Frozen v1 engagement has no idempotency key or atomic engage-if-absent operation. Every enqueue therefore requests an engagement; the named `task-queue` claim and `queue=True` serialize races durably. Once the first engagement drains the journal, redundant queued engagements observe no waiting task and immediately return `omp.Done()`.