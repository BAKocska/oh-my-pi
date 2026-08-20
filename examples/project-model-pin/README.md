## What the pi original did

`pi-set-model` saved the active provider model and thinking level in `<cwd>/.pi/set-model.json`, restored them when a session opened in that folder, and exposed `/set-model` operations to view, replace, or delete the file. It also restored the pre-project selection at shutdown (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:299`; the origin package is `hknet/pi-extensions/packages/pi-set-model`).

## The omp shape

`/pin_model` appends the active typed `omp.ModelRef` to `omp.state` at `StateScope.PROJECT`. PROJECT scope is already keyed by Core to the authenticated principal and normalized workspace, so the per-folder JSON, path construction, trust check, atomic rename, and shutdown-time global-model restoration are deleted. There is no filesystem access or path-keyed state. This follows `docs/py/09-journal.md` §`omp.state`, especially `StateScope.PROJECT` and `state.latest`.

A `turn_start` TRANSFORM reads that project pin and returns `omp.Modify(patch={"model": ...})` only when the turn differs. A `model_changed` OBSERVE hook treats `ModelChangeReason.USER` as an explicit session override and appends a disabled `PinLatch` at `StateScope.SESSION`; later turns defer instead of fighting the user. Running `/pin_model` records the current model and enables the latch again. The typed event and mutable-field contracts are in `docs/py/05-hooks.md` §§3.3 and B.

## Gaps

- The requested route pin cannot be recorded by `/pin_model`: frozen `omp.Context` exposes `model` but no current `RouteRef` (`crates/py/python/omp/_context.py:32-54`), while the command ABI provides only `(ui.Invocation, Context)` (`docs/py/07-ui.md` §4.15). `TurnStartEvent.route` exists (`crates/py/python/omp/events.py:400-414`; `docs/py/05-hooks.md:1220-1233`), but it arrives only later and is not the selection at command invocation. The port therefore applies the typed model patch and does not fabricate a route id from `ModelRef.api`.
- The pi original's thinking-level pin has no frozen current-selection or turn mutation surface. `omp.Context` has no thinking field (`crates/py/python/omp/_context.py:32-54`), `TurnStartEvent` has no thinking field (`crates/py/python/omp/events.py:400-414`), and the authoritative mutable-field list permits only `turn_start.{model, route, deadline}` (`docs/py/05-hooks.md:1339-1341`).
