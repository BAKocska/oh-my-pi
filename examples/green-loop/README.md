# Green loop

## What the pi original did

`pi-green-loop` kept a local watch-mode daemon alive and repeatedly selected affected tests for Node, Python, Go, Rust, and Make projects. Its goal was CI-shaped feedback while an agent edited files: remember the changed paths, run only the relevant local checks, and make a regression visible before a full remote CI run.

## The omp shape

The daemon, filesystem watcher, polling loop, timer, timestamp file, and parallel cache are deleted. A `tool_result` OBSERVE hook records only settled successful core `edit` and `write` paths as typed `StateScope.SESSION` entries. Each `_GreenRun` entry is also the watermark for the next `state.entries(..., since=...)` fold, so the session journal remains the only truth (`docs/py/05-hooks.md:1421-1584`, *Call events*; `docs/py/09-journal.md:535-603`, *omp.state*).

An idempotently upserted `omp.agents.AfterIdle` schedule injects a private sentinel after the configured quiet period. A schedule-origin `before_agent_start` REVIEW hook consumes that sentinel, runs the selected command once through `omp.env.sh.run`, and denies the now-handled submission before it reaches the model. `/green` calls the same path on demand. This is the local half of CI rather than a resident CI imitation: Core owns idle detection and disarms it on user input, while the Environment owns the bounded command (`docs/py/12-agents.md:831-901`, *Schedules, timers, and durable delivery*; `docs/py/11-env.md:967-991`, `omp.env.sh.run`).

`[settings].test_runner` can override the runner with a `{paths}` template. Otherwise `[settings].affected` is the origin's polyglot table: named entries contain path globs and a command template for Node, Python, Go, Rust, or Make. A successful run paints the keyed green status badge. A failed run paints it red and emits one bounded `omp.agents.inject(..., role="system")` notification only when its normalized failure digest differs from the preceding run, preventing unchanged idle failures from nagging (`docs/py/07-ui.md:978-1011`, statusline and notices; `docs/py/12-agents.md:637-646`, injection on the frozen surface).

## Gaps

- `omp.agents.Inject`: the frozen dataclass requires `prompt: str` at `crates/py/python/omp/agents.py:818-824`, while `docs/py/12-agents.md:872-882`, *Delivery targets*, documents `Inject(mode=..., visible=...)` without that required field even though the prose says a prompt is delivered.
- `omp.BeforeAgentStartEvent.schedule_id`: the frozen payload includes `schedule_id: str | None` at `crates/py/python/omp/events.py:377-388`, but `docs/py/05-hooks.md:1203-1213`, *Turn and submission lifecycle*, omits it from the documented payload.
