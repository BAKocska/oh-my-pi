## What the pi original did

`@mrclrchtr/supi-cache` monitored prompt-cache hit rates, persisted turn records, and warned when cache performance regressed. It also computed its own structural prompt fingerprint with a hand-rolled hash and rebuilt causal state through several lifecycle hooks.

## The omp shape

This port uses the assembler-owned `PromptFingerprint.changed` and `prefix_stable_bytes` fields plus `Usage.cache_read` directly, so it contains no hashing or fingerprint reconstruction. One replaying telemetry sink replaces the original lifecycle state machine, writes typed `CacheTurn` entries idempotently, and uses the frozen synchronous `omp.ui.notify` surface; `cache_report` folds the most recent entries in ascending journal order. Replay follows the corrected contract in `docs/py/10-telemetry.md` §Reference and §2: snapshot at a watermark, reverse the recorded newest-first delivery mistake into chronological oldest-first delivery, then atomically switch to live events. The counter and histogram use extension-relative names; §quotas forces them under `omp.ext.examples.cache-monitor.*` and bounds instrument count and attribute cardinality.

## Gaps

None. The frozen `ui.Level` accepts `"warning"` as the `WARN` alias, and the manifest schema ratifies `[[telemetry]]` with `kinds`, `scope`, `queue`, and `overflow`.
