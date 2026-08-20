# Plan mode

## What the pi original did

`@dreki-gg/pi-plan-mode` ran a two-phase plan-ledger workflow: it switched to a planning model, replaced the active tools with a read-only subset, blocked unsafe shell calls, rewrote stale context across transitions, and exposed a loopback plan viewer before execution continued in a clean session.

## The omp shape

The `plan` soft tool appends typed enter/exit transitions to SESSION-scoped `omp.state`; status is rebuilt by folding that log, and exit appends the completed `Plan` to the same session journal before leaving planning mode. While active, a fail-closed `tool_call` PRECHECK returns `Deny(code="plan_readonly")` for core `write`/`edit` calls and for bash whose Core-supplied `BashIR.is_read_only()` is false. A `turn_start` TRANSFORM changes only the planning model and thinking selection. It never patches `enabled_tools` or otherwise mutates the tool array, which remains byte-identical across transitions; this deletes the pi extension's per-transition tool re-registration and its prompt-cache churn. Client-side context rewriting and the loopback HTTP viewer are also deleted (`docs/py/05-hooks.md` §4.3, “Two-phase plan mode”).

## Gaps

- `omp.BashIR` is required by `ToolCallEvent.bash` and `BashIR.is_read_only()` in `docs/py/06-policy.md` §4 and the worked port in `docs/py/05-hooks.md` §4.3, but `crates/py/python/omp/__init__.py` does not import or export it; `crates/py/python/omp/events.py` only names it as an unresolved postponed annotation.
- `omp.ModelRef` is the documented type of `TurnStartEvent.model` in `docs/py/05-hooks.md` §3.3, but `crates/py/python/omp/__init__.py` does not export it and `crates/py/python/omp/events.py` only names it as an unresolved postponed annotation.
- `turn_start.thinking` is required to switch model and thinking together, but frozen `TurnStartEvent` in `crates/py/python/omp/events.py` has no `thinking` field, and the mutable-field contract in `docs/py/05-hooks.md` §3.3 lists only `turn_start.{model, route, deadline}`. The extension emits the requested `Modify.patch` key, but the frozen event contract cannot validate or apply it.
