## What the pi original did

`pi-true-queue` held later tasks out of the agent's context until the current task finished, then released exactly one queued task. This prevented a running task from being distracted by future work.

## The omp shape

The `queue_task` device and `/queue_task` command append typed `QueuedTask` transitions to the session journal. They inspect their own active campaigns and engage the Session-scoped `task-queue-drain` campaign only when it is absent, without adding a queued engagement ticket, using the runtime surface in docs/py/15-campaigns.md §4.0. The drain needs no exclusive campaign slot. The entry kind deliberately has no `project` method, and the command returns an empty `ui.Consumed()` rather than a `ui.Prompt` or notice, so waiting task text occupies no prompt slot, message, or other context item (docs/py/09-journal.md, “Recognized methods on the decorated class”; docs/py/07-ui.md §4.15).

At `omp.SETTLE`, the campaign folds the journal. Its journaled `QueueCampaignState` remembers the task handed out by the last committed reaction, so only a subsequent settle for that engagement can mark that task done. The same reaction may then emit `omp.Continue(inject=<next task>)` for the oldest waiting entry and return that task id as its next state. A drained queue emits `omp.Done()`. This is doc-15 §1's takeover skeleton and §6's “Todo completion reminder” porting shape expressed through the frozen verdict vocabulary (§2.2, §4.0-4.5), rather than a domain hook that hand-rolls settlement control. Journaled transitions reconstruct ordering after process replacement, while campaign state advances only with a committed reaction. `omp.Ladder(8)` provides the campaign-local finite bound; Core's `ContinuationLedger` remains the global backstop underneath it as described in §2.3.

Unlike `goal-loop`, which repeatedly tests and advances one goal until a completion predicate is met, this extension is a work queue: every entry runs once, in insertion order, under strict serialization.

## Gaps

- The Rust agent loop's SETTLE consumer currently substitutes `AutoContinue` and does not yet deliver `Continue.inject`; this is a recorded agent-side gap.
- Delivered-effect stepping from doc-15 §2.4 is not observable from the frozen v1 handler surface. The example uses a subsequent settle plus committed journaled campaign state as its honest proxy, so it never marks a task done in the reaction that handed it out, but delivery-aware anti-windup remains an agent-side gap.
- Frozen v1 engagement has no idempotency key or atomic engage-if-absent operation. The enqueue paths avoid durable redundant tickets by checking their own active engagements before engaging without `queue=True`; simultaneous enqueue calls can still race across that check.