## What the pi original did

`pi-schedule-prompt` let users schedule recurring or one-shot prompts for the main agent or an isolated background agent. It stored cron entries in a JSON file, reloaded them on session start, and evaluated them from an in-process interval. That made a firing depend on the extension process still being alive.

## The omp shape

The two soft devices are reached through the core `dyn` tool (`{"do_": "invoke/schedule_prompt"}` and `{"do_": "invoke/schedules_list"}`). `schedule_prompt` performs an idempotent upsert on `(owner, name)` using `Cron`, explicitly keeps the `COALESCE` missed-run default, sends main-agent deliveries to `Inject`, and uses `Spawn(SubagentSpec(background=True))` for isolated work. A project-scoped isolated firing must carry a positive per-firing `ScheduleBudget`; the principal captured by the core at declaration owns and pays for its firings.

There are no timers, interval loops, state files, activation hook, or local firing ledger. As `docs/py/12-agents.md` §Scheduling puts it, a timer in a process that exits is an intention, not a schedule: the core scheduler and journal own firing truth, missed-run recovery, and the `(schedule_id, scheduled_at_ms)` firing idempotency key. `schedules_list` reads that durable projection back rather than reconstructing it locally. Project-scoped upserts use the ratified `schedules:project` manifest capability.

## Gaps

- The documented `Inject(mode, visible)` signature has no prompt or payload parameter. The non-isolated branch can declare an injected firing target, but cannot attach `SchedulePromptArgs.prompt`; that content path must be added to the schedule contract rather than hidden in a state file.
