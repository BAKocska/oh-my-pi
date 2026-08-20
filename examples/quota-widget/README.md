# Quota widget

## What the pi original did

`@benvargas/pi-synthetic-provider` exposed provider quota through its own command, while `@ogulcancelik/pi-minimal-footer` rendered the remaining allowance in terminal chrome. The pi shape relied on extension-owned quota transport and persisted cache state.

## The omp shape

The usage-only `synthetic-provider` declaration advertises `Operation.USAGE`; its provider-scoped `provider_usage` projection returns request and token `UsageWindow` rows. Both those rows and the `/quota` command are folded from `omp.sessions.usage()`, so spend comes from durable per-turn receipts rather than the droppable telemetry firehose. A keyed `STATUS_RIGHT` segment shows the tightest remaining window in semantic `ok`, `warn`, or `err` theme colors, while `/quota` returns the full aligned TML table (`docs/py/13-inference.md` §“provider_usage”; `docs/py/09-journal.md` §“async omp.sessions.usage(query)”; `docs/py/07-ui.md` §§4.4, 4.15).

The private provider HTTP client and extension-owned cache file are deleted. Core owns provider usage resolution, durable receipt aggregation, refresh timing, retained status composition, command routing, TML layout, and theme colors.

## Gaps

- `provider_usage` return handling diverges: the frozen layer classifies it as an observation event in `crates/py/python/omp/hooks.py:280-290`, while `docs/py/13-inference.md:1266-1275` and `docs/py/13-inference.md:1490-1522` specify a provider-scoped hook returning `UsageReport | None`. The example returns the documented report, but the frozen observation dispatch contract may discard it.
- The documented top-level `@omp.command` is absent: `crates/py/python/omp/__init__.py:230-239,291-293` exports `ui` and only re-exports `renderer`, while the available decorator is `omp.ui.command` at `crates/py/python/omp/ui/__init__.py:653-656`. This example uses the frozen spelling; `docs/py/07-ui.md:376-378,1568-1576` requires the top-level spelling.
