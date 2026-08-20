## What the pi original did

`@danypops/pi-packed` managed packages and resources through an authenticated daemon and also supplied named profiles. Its profile half switched the model, thinking level, active tools, instructions, and theme together, then persisted the selected profile as a custom session entry (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:28`).

## The omp shape

Profiles are ordinary `[settings]` data. Each table contains a typed `ModelRef`, `RouteRef`, portable `Effort`, and a complete set of device paths. The top-level `@omp.command("profile", ...)` declaration supplies a `<profile>` usage ghost and dynamic completion over those configured names (`docs/py/07-ui.md` §4.15). Selecting a profile sends one sorted batch of `AvailabilityDelta` values through `omp.devices.set_availability`, appends exactly one typed `ProfileApplied` entry, and makes the latest journal entry authoritative for the `turn_start` TRANSFORM's single `Modify` patch of `model` and `route` (`docs/py/05-hooks.md` §3.11 family B; `docs/py/09-journal.md` §Reference). No extension-owned state file or parallel cache exists.

The device transition changes availability only. It never registers, unregisters, or replaces tools. This preserves the cache rule in `docs/py/01-devices.md` §“Availability is a notification, not a re-registration”: the model's tool array remains byte-identical while one system notice carries the catalog delta. In particular, there is no `setActiveTools` equivalent.

Theme switching is intentionally not ported. `docs/py/07-ui.md` §6.4 permanently drops extension `setTheme`/`getAllThemes`/`getTheme`; themes are a user-owned presentation choice, not profile state.

## Gaps

- `TurnStartEvent.thinking` is absent from the frozen event at `crates/py/python/omp/events.py:399-414`, and the mutable-field contract lists only `turn_start.enabled_tools` plus `turn_start.{model, route, deadline}` at `docs/py/05-hooks.md:1339-1340`. Although `omp.Effort` is frozen in `crates/py/python/omp/provider.py:200-209` and documented at `docs/py/13-inference.md:828-830`, no parent-turn patch field consumes it. The profile validates and durably records its thinking level, but cannot apply that field until the hook payload and composition contract add `turn_start.thinking`.
