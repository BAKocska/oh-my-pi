# Sidebar

## What the pi original did

`@esso0428/pi-sidebar` painted a floating right sidebar with live git and session facts. Its centerpiece was a compositor takeover: `Object.defineProperty(terminal, "columns", …)` lied about terminal width, wrapped `doRender`, and wrote cursor-positioning and terminal-mode escapes after every host render. That made wrapping, scrollback, and composition with a second sidebar impossible to make correct from inside the extension.

Origin: `@esso0428/pi-sidebar` v0.1.36, cataloged in `.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`.

## The omp shape

This port mounts one retained `Slot.SIDEBAR_RIGHT` rail and never touches terminal geometry or output. `SlotOptions(width=34, min_width=120, min_height=20, collapse=Collapse.SHRINK)` gives the layout solver the old thresholds as overridable preferences. `order=200` only seeds the default: user layout preferences remain authoritative in multi-rail arbitration.

Tree-scoped session/turn telemetry folds into a bounded live tree and facts panel. The subscription coalesces by `(kind, session)` under pressure, while the UI's keyed state effects coalesce by `(extension, key)`. A BLAKE2s digest short-circuits identical rendered facts; changed states patch only changed text-bearing ids rather than reparsing the rail. All markup is constructed with frozen `ui.tml`, `ui.text`, and `ui.icon`.

See `docs/py/07-ui.md` §2.8, §4.6, and the worked port in §5.3; telemetry scope and overflow are defined in `docs/py/10-telemetry.md` §3 and §4.

Deleted mechanisms: `Object.defineProperty(terminal, …)`, the `doRender` wrapper, raw cursor addressing, synchronized-output fences, autowrap toggles, cursor save/restore, and extension-owned width arbitration.

## Gaps

- `omp.telemetry.Envelope`, `SessionStart`, `TurnStart`, `TurnEnd`, and `SessionEnd` are specified in `docs/py/10-telemetry.md` §4 (`Envelope` at lines 690-708 and concrete events at lines 716-779), but the frozen export list omits them at `crates/py/python/omp/telemetry.py:458-463`. The port therefore accepts the documented event objects as `object` and reads their documented fields without public concrete annotations.
