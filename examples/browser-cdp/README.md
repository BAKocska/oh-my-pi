# Browser CDP

## What the pi original did

`@narumitw/pi-chrome-devtools` exposed Chrome DevTools Protocol tools for page navigation, JavaScript evaluation, inspection, and screenshots. It dynamically changed which registered tools were visible with `pi.setActiveTools`, based on configuration or a TUI choice, so each enabled operation occupied its own model-facing schema slot (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:81`).

## The omp shape

One soft `browser` device owns the static `open`, `eval`, `snapshot`, `screenshot`, and `close` children. Calls use child paths such as `dyn {"do_":"invoke/browser/open", ...}`; each leaf keeps its own argument schema and documentation in the catalog, fetched through `dyn docs/browser/<leaf>`, rather than claiming five permanent model schema slots. The fixed `dyn` tool array is byte-identical while browser availability changes. This deletes `setActiveTools`, visibility toggling, and per-operation model tool registration (`docs/py/01-devices.md` §§“The three ops, end to end”, “Availability is a notification, not a re-registration”, and “`omp.Device`”).

`extension_activate` uses `omp.env.proc.ensure` to adopt or start one Environment-owned named Chromium process. `ReadyTcp` observes the debugging port before any child is enabled, `RestartPolicy(omp.Restart.ON_FAILURE)` delegates crash recovery, and CDP traffic uses the generation-fenced `Process.endpoint`; no PID file, poll loop, or hand-rolled browser restart exists (`docs/py/11-env.md` §§“`await omp.env.proc.ensure`”, “`class omp.env.Process`”, and “Process value types”). The process-state stream publishes the parent and all five children in one `omp.devices.set_availability` batch. A down transition therefore disables the whole device through one `TurnBoundary` catalog delta rather than interrupting an in-flight tool or mutating the model tool array (`docs/py/01-devices.md` §§“Availability transitions” and “`crates/agent` — one notification, and no gate at all”).

Long `browser/open` calls yield typed `Update` frames for target creation and navigation lifecycle events, followed by exactly one `Done`. Accessibility snapshots are explicitly node-bounded. `browser/screenshot` decodes CDP image bytes into a media-typed `omp.Spill` held by an `omp.BlobPart`; neither inline base64 nor a temporary path reaches the verdict (`docs/py/01-devices.md` §“One body contract”; `docs/py/02-verdicts.md` §“`omp.BlobPart`”; `docs/py/11-env.md` §“Blobs — `omp.env.blobs`”).

Deleted mechanisms: `setActiveTools`, TUI-owned visibility state, five model-facing schema slots, unmanaged Chrome processes, manual readiness polling, inline screenshot base64, and temporary screenshot files.

## Gaps

None — every symbol this port needs is frozen.
