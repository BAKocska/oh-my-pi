## What the pi original did

The cataloged `@narumitw/pi-usage` and `@sreetej510/pi-usage` extensions displayed provider-account usage and rate-limit budgets through commands and live status indicators (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:147-148`). This derived port asks the corresponding host-local question: how much of this extension's own CONTROL budget has the host admitted, dropped, or left available?

## The omp shape

`resource_receipts` calls `omp.resources()` and projects every `ResourceReceipt.quotas` row into its core-reported limit, consumed amount (`QuotaStatus.used`), saturating remaining amount, optional `Duration` window, and matching soft-drop count. With `record_probe=false` it is a nanosecond local read with no effect. With `record_probe=true` it consults the live `journal.appends` row before attempting one typed journal append, then rereads the receipt after admission. A zero remaining budget or a raced `omp.QuotaExceeded` becomes the typed `disposition="deferred"` result; the extension does not sleep, retry, evict work, or guess when the host's window becomes eligible (`docs/py/00-overview.md:367-374,760-780`, §“Quotas and fairness” and §`omp.resources() -> ResourceReceipt`).

The source of truth is therefore the host-pushed receipt, not extension state. This deletes both local accounting patterns visible in earlier ports: `examples/tool-toggle/tool_toggle.py:26-38,92-97`'s extension-authored cost normalization (whose README explicitly refuses to invent a missing `catalog_notice_tokens` counter) and `examples/segment-bus/segment_bus.py:9-13,62-72,143-165`'s hard-coded maxima plus custom `PublisherQuotaExceeded`. There is no module counter, token bucket, timestamp queue, quota constant, or extension-defined quota exception here. `ReceiptProbe` records only an admitted observation; it is not consulted for accounting.

The receipt reader itself is frozen end to end: `_omp` defines immutable `QuotaStatus` and `ResourceReceipt` views and the local `resources()` snapshot at `crates/py/src/bindings.rs:986-1030`; `crates/py/python/omp/_registry.py:18-25` imports them unchanged, and `crates/py/python/omp/__init__.py:393-405` exposes them at top level.

## Gaps

None — every symbol this port needs is frozen.
