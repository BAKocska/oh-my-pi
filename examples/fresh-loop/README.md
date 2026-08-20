## What the pi original did

`@cjvnjde/pi-fresh-loop` ran the same prompt repeatedly, starting a fresh Pi session for every attempt, until an output satisfied its stop condition.

## The omp shape

The port exposes one soft `fresh_loop` tool. Each attempt uses `omp.agents.spawn(SubagentSpec(..., isolation=Isolation.CLEAN))`, so the child receives its system prompt and task but none of the parent's conversation. It deliberately does not use `Isolation.FORK`: a fork copies the parent's live chain and charges that whole context on the child's first turn, defeating the original's fresh-session semantics and making the first-turn cost depend on parent history (docs/py/12-agents.md, “Spawning”). `max_depth=0` makes each attempt a leaf, while `Budget` puts hard request, token, cost, and wall ceilings on it; Core recursively clamps those ceilings against every ancestor rather than trusting extension bookkeeping.

A plain `stop` is a substring predicate over the settled `SubagentResult.text`. Supplying `choices` instead treats `stop` as the terminal choice and sends the child's typed status, text, structured data, and output URL through `omp.agents.completion`; the first non-stop choice is the explicit fail-safe default (docs/py/12-agents.md, “The handle” and “One-shot completions”). `RunStatus.EXHAUSTED` is journaled and returned as `budget_exhausted` without another paid classifier call or another spawn.

Every settled attempt appends a typed `FreshLoopIteration` containing the child session id, lifecycle status, verdict, child and classifier spend, fallback flag, and durable `agent://` output pointer. The loop identity is either a hash of `resume_key` or a stable hash of the prompt and stop configuration. A rerun folds its live history with `omp.journal.entries`, starts at the next iteration number, and immediately returns an already-terminal verdict; `max_iterations` is a total durable ceiling, not a fresh allowance on every call (docs/py/09-journal.md, “`omp.journal`”).

This differs from `examples/goal-loop`: goal-loop returns `Continue` at the settled-turn boundary and keeps working in the same session and continuation ledger, preserving context. Fresh-loop instead pays for a new `Isolation.CLEAN` child on every iteration and never continues a prior child.

## Gaps

None — every symbol this port needs is frozen.
