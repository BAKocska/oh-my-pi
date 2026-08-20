# What the pi original did

[`pi-tbox`](https://www.npmjs.com/package/pi-tbox) listed tools from every loaded extension, grouped and focused the list, toggled active tools, persisted the choice in chat state, and displayed the context cost of the active set.

# The omp shape

`/tool-toggle [focus]` uses the native grouped `ui.multi_select` overlay (`docs/py/07-ui.md` §4.10) rather than shipping a terminal component. A focus term matches the tool path or any of the structurally stamped provenance septet — publisher, extension id, version, artifact digest, layer, trust tier, and generation — and every picker group renders all seven fields.

One accepted picker result becomes one tuple of `omp.AvailabilityDelta` values and one `await omp.devices.set_availability(*deltas)` call. It never unregisters or re-registers tools. Core therefore owns the atomic transition, journal item, catalog notice, and `TurnBoundary` delivery; `Immediate` is specifically forbidden because a catalog change must not preempt a running call (`docs/py/01-devices.md` §“Availability transitions” and §“one notification, and no gate at all”, lines 2058–2099). The complete selected-path snapshot is appended to SESSION-scoped `omp.state` and restored on `extension_activate`.

Each row makes the cost asymmetry explicit. A granted schema-slot tool shows the catalog's schema byte and token counts. A `dyn` device shows `0 B · 0 tokens`: catalog and docs are content read on demand, not a model-facing request schema (`docs/py/01-devices.md` §Purpose, lines 61–78, and §`dyn`, lines 1145–1164). This asymmetry is the lesson, not a display shortcut.

The overlay also states the accounting caveat from `docs/py/08-context.md` open question 5, lines 2091–2102: an availability change's notice is a message-list item, but whether its cost should also be exposed as `catalog_notice_tokens` remains unresolved. The picker therefore reports catalog schema cost only and does not invent that fourth counter.

# Gaps

- `omp.devices.list(*, mounted_only: bool = True)` is documented at `docs/py/01-devices.md:967-975`, but the frozen `Devices` class in `crates/py/python/omp/devices.py:356-394` exposes only `parent`, `set_availability`, `enable`, `disable`, and `refresh`. Exact missing symbol: `omp.devices.list`.
- The picker contract needs each catalog row's full stamped provenance septet, slot-vs-device presentation, and precomputed schema byte/token cost. Frozen `omp.DeviceInfo` at `crates/py/python/omp/devices.py:189-209` contains only `claimant` and `source` for origin and has no presentation or cost fields; the documented field list at `docs/py/01-devices.md:1018-1025` has the same omission even though the catalog is required to show provenance (`docs/py/01-devices.md:42-44`) and hard slots are explicitly priced (`docs/py/01-devices.md:61-78`). Exact missing symbols required by this example: `omp.DeviceInfo.provenance: omp.Provenance`, `omp.DeviceInfo.slotted: bool`, `omp.DeviceInfo.schema_bytes: int`, and `omp.DeviceInfo.schema_tokens: int`.

Until those frozen catalog gaps close, `_catalog_rows()` fails explicitly instead of fabricating provenance or recomputing provider schema cost in extension code. The delta, persistence, grouping, focus, and picker logic remain executable against the stated catalog-row contract.
