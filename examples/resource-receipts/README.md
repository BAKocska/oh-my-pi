## What the pi original did

The cataloged `@narumitw/pi-usage` and `@sreetej510/pi-usage` extensions displayed provider-account usage and rate-limit budgets through commands and live status indicators (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:147-148`). This derived port asks the corresponding host-local question: how much of this extension's own CONTROL budget has the host admitted, dropped, or left available?

## The omp shape

`resource_receipts` calls `omp.resources()` and projects every `ResourceReceipt.quotas` row into its core-reported limit, consumed amount (`QuotaStatus.used`), saturating remaining amount, optional `Duration` window, and matching soft-drop count. With `record_probe=false` it is a nanosecond local read with no effect. With `record_probe=true` it consults the live `journal.appends` row before attempting one typed journal append, then rereads the receipt after admission. A zero remaining budget or a raced `omp.QuotaExceeded` becomes the typed `disposition="deferred"` result; the extension does not sleep, retry, evict work, or guess when the host's window becomes eligible (`docs/py/00-overview.md:367-374,760-780`, §“Quotas and fairness” and §`omp.resources() -> ResourceReceipt`).

The source of truth for *this extension's own consumption* is therefore the
host-pushed receipt, not extension state: there is no module counter, token
bucket, timestamp queue, quota constant, or extension-defined quota exception
here, and `ReceiptProbe` records only an admitted observation rather than
feeding accounting. Two earlier ports were reconciled against that boundary,
and the boundary is narrower than a first reading suggests. `segment-bus/`
dropped its custom `PublisherQuotaExceeded` for `omp.QuotaExceeded` — the
*mechanism* the receipt does delete — but keeps
`MAX_SEGMENTS_PER_PUBLISHER`/`MAX_BYTES_PER_PUBLISHER`, because those divide
the slot *it owns* among sibling callers, and a core receipt describing this
extension's standing says nothing about how a service owner should apportion
its own capacity per caller. `tool-toggle/` needed no change: its
`schema_bytes`/`schema_tokens` already come from the frozen `DeviceInfo`
catalog rows, and a per-extension receipt could not supply per-device schema
cost in any case. The rule the two cases share: the receipt replaces
self-accounting, never application-level admission policy or catalog
metadata.

The receipt reader itself is frozen end to end: `_omp` defines immutable `QuotaStatus` and `ResourceReceipt` views and the local `resources()` snapshot at `crates/py/src/bindings.rs:986-1030`; `crates/py/python/omp/_registry.py:18-25` imports them unchanged, and `crates/py/python/omp/__init__.py:393-405` exposes them at top level.

## Gaps

None — every symbol this port needs is frozen.
