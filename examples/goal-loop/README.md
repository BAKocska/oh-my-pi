## What the pi original did

`@narumitw/pi-goal` registered an objective, watched turn lifecycle events, and injected follow-up messages until it considered the goal complete. It also implemented its own progress and repetition bookkeeping, which could race other continuation extensions and relied on interpreting message text.

## The omp shape

The soft `goal` device keeps the original `goal_set`, `goal_status`, and `goal_complete` operations and its typed Session snapshots. `goal_set` now engages the Session-scoped `goal-loop` campaign with journaled `GoalCampaignState`; `goal_complete` records completion and disengages that campaign.

At `SETTLE`, the campaign vetoes stopping with `Continue(inject=...)` while the objective is unmet, returns `Pass()` to accept a transient stall without ending the Session engagement, and returns `Done()` only when the goal is complete or replaced. This is the first-class veto-the-stop skeleton from docs/py/15-campaigns.md §1, follows the `Continue` law in §2.2 and empty-fold `SETTLE` behavior in §3.3, and is the §6 `session_stop hook` porting shape. The old `agent_settled` hook, local loop return types, and manifest hook row are gone. Core's continuation ledger remains the global backstop rather than campaign-private retry bookkeeping.

## Gaps

- The standing `Until`-bounded policy and `Interlock` described in docs/py/15-campaigns.md are not in the frozen v1 surface. This example instead keeps one Session engagement active, emits the available `Continue` verdict while work remains, accepts a transient stall with `Pass`, and terminates the engagement with the available `Done` verdict only on completion or replacement.
- The `SETTLE` consumer currently substitutes `AutoContinue` and does not yet deliver the `Continue.inject` payload; this is a recorded agent-side gap.
