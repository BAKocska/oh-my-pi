## What the pi original did

`pi-true-queue` held later tasks out of the agent's context until the current task finished, then released exactly one queued task. This prevented a running task from being distracted by future work.

## The omp shape

The `queue_task` device and `/queue_task` command append typed `QueuedTask` transitions to the session journal. The entry kind deliberately has no `project` method, and the command returns an empty `ui.Consumed()` rather than a `ui.Prompt` or notice, so waiting task text occupies no prompt slot, message, or other context item (docs/py/09-journal.md, “Recognized methods on the decorated class”; docs/py/07-ui.md §4.15). At each `agent_settled` boundary, the hook marks the one started entry done, folds the journal again, and returns `Continue(prompt=<next task>, collapse_prior=True)` for only the oldest waiting entry; an empty queue returns `Settle()` (docs/py/05-hooks.md §4.2; docs/py/12-agents.md, “Autonomous loops”). The journal is the only truth, so process restarts replay the same queue. Continuation capacity comes only from Core's durable `ContinuationLedger`, and an accepted `Continue` still respects `defer_interrupts`.

Unlike `goal-loop`, which repeatedly tests and advances one goal until a completion predicate is met, this extension is a work queue: every entry runs once, in insertion order, under strict serialization.

## Gaps

- `omp.agents.Settle`: frozen `crates/py/python/omp/agents.py:184-187` is a fieldless dataclass, but `docs/py/05-hooks.md` §4.2 calls `Settle(reason=...)`.
- `omp.agents.Continue`: frozen `crates/py/python/omp/agents.py:173-181` requires `prompt: str`, but `docs/py/05-hooks.md` §4.2 passes the nonexistent `omp.Item.user_note(...)`.
- `omp.journal.append` / `omp.journal.state`: frozen `crates/py/python/omp/journal.py:65-74,133-146` defines synchronous `append` and exports no `state`, but `docs/py/05-hooks.md` §4.2 awaits both `omp.journal.append(...)` and `omp.journal.state(...)`.
