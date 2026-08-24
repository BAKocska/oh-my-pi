# Tool search

## What the pi original did

`pi-tool-search` hid registered tool schemas behind a manifest-aware `tool_search`, then let the model enable selected tools by name on demand.

| Origin mechanism | Harness replacement |
|---|---|
| Manifest-aware `tool_search` | The `xd` shell builtin already provides catalog listing and search (`xd`, `xd --q <text>`), docs (`xd <path> --help`), and invocation (`xd <path> [args…]`). |
| Hidden tools loaded into model slots on demand | Soft devices stay host-registered in the device catalog behind `xd` and consume zero model-facing schema slots. |
| `setActiveTools`-style enable/disable | `omp.devices.set_availability(AvailabilityDelta(...))` emits availability notices without changing the request tool array. |

## The omp shape

The searchable, low-context premise is built into the harness: the `xd` shell builtin owns catalog search, detailed docs, and invocation, while catalog reads remain content rather than request schema ([Devices — Purpose and transport](../../docs/py/01-devices.md#purpose)). There is therefore no second search tool in this port.

`tool_enable` and `tool_disable` are soft devices that accept one path from the comma-separated `[settings.allowlist]`. They inspect the session catalog and lower accepted changes to a single `omp.AvailabilityDelta`; the host delivers that transition as an availability notice at `TurnBoundary`. The advertised tool array remains byte-identical across these changes ([Devices — Availability is a notification, not a re-registration](../../docs/py/01-devices.md#availability-is-a-notification-not-a-re-registration)).

A slotted catalog entry is refused. Availability cannot promote or mutate a model-facing hard slot: hard intent is an install-time, named `tools.hard` grant and is also constrained by the session slot budget ([Devices — Soft and hard](../../docs/py/01-devices.md#omptool), [Deploy — capability vocabulary](../../docs/py/14-deploy.md#392-the-capability-vocabulary-and-how-it-surfaces)).

Configure a bounded set explicitly, for example:

```toml
[extensions.settings."examples.tool-search"]
allowlist = "repo_search,issue_lookup"
```

Then invoke `tool_enable` or `tool_disable` with `xd tool_enable <path>` or `xd tool_disable <path>` in the shell; neither device registers, unregisters, or rewrites any target tool schema.

## Gaps

- `omp.devices.list`: the frozen surface is `async def list(...)` (`crates/py/python/omp/devices.py:408`), while [Devices — `omp.devices`](../../docs/py/01-devices.md#ompdevices) documents `list(...) -> tuple[DeviceInfo, ...]` without `async` (`docs/py/01-devices.md:972`).
- `omp.devices.HARD_SLOT_BUDGET`: [Devices — `omp.devices`](../../docs/py/01-devices.md#ompdevices) documents the constant (`docs/py/01-devices.md:998`), but the frozen `Devices` namespace does not define it (`crates/py/python/omp/devices.py:368-415`).
