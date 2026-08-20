## What the pi original did

`pi-preemptive-compact` and the compaction half of `pi-harness-runtime` watched context occupancy and proactively started compaction at a configured percentage. The runtime also scraped provider web consoles for subscription quotas.

## The omp shape

The `turn_start` OBSERVE hook reads the agent-maintained `omp.context.usage()` snapshot and compares `ContextUsage.fraction` with `[settings].threshold`. A durable journal ledger implements a `[threshold - hysteresis, threshold]` band: one crossing requests `omp.context.compact(tier=omp.CompactionTier.LOCAL)`, subsequent turns in or above the band are held, and the trigger rearms only after pressure falls through the lower edge. `CompactionBusy` is journaled and otherwise treated as a no-op. This uses the sanctioned durable request described in [08-context.md, `omp.ContextEpoch` and `omp.context`](../../docs/py/08-context.md#ompcontextepoch-and-ompcontext), while the hook placement and OBSERVE side-effect rule come from [05-hooks.md §3.4](../../docs/py/05-hooks.md#34-the-five-phases).

The `compaction` domain hook offers a cheap, bounded `CustomSummary` at the LOCAL rung only when this extension has trigger-ledger rows and the host-suggested `first_kept_id` is valid. It preserves the preceding summary, folds bounded `MessageRef.preview` values rather than pulling bodies or paying for inference, records its decision, and otherwise returns `None` so the harness rescue ladder proceeds ([08-context.md, Compaction control](../../docs/py/08-context.md#compaction-control)). The journal is the only hysteresis and summary-ledger truth; process-local state is not authoritative.

This differs from `observational-memory`: that example owns a semantic observation ledger and therefore owns the summary; this extension owns the pressure trigger and only contributes a summary when its small trigger ledger can accompany the host's bounded context previews. Provider-console quota scraping is deliberately deleted. Provider consoles, cookies, and subscription dashboards are not an extension's business; provider usage belongs on the typed provider/telemetry surfaces rather than browser scraping.

## Gaps

None.
