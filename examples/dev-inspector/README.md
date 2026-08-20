# Developer inspector

## What the pi original did

`pi-dev-inspector` exposed the assembled system prompt and captured provider API request/response round-trips for interactive debugging.

## The omp shape

`/inspect prompt` opens a read-only overlay over the latest assembler-owned `PromptFingerprint`: slot keys and changed flags come directly from the fingerprint, and the implementation never hashes prompt material. Stability labels follow the fixed prompt slot catalog in `docs/py/08-context.md` §3; unknown assembler keys remain explicitly unknown rather than guessed. `/inspect request` queries recent `ModelRequest` rows for only the active session and shows the served model, total tokens, and cache hit rate (`docs/py/10-telemetry.md` §2).

The request overlay checks the invocation's actual `telemetry.capture_content` grant before rendering `Tokens.detail`. Without it, the overlay always renders an explicit redaction notice. Capture is a grant, not a default: `Capture.CONTENT` additionally requires an explicit durable user grant and is never implied by trust tier (`docs/py/10-telemetry.md` §4). The port is read-only: it registers no hooks, telemetry sinks, journal entries, devices, or environment effects.

## Gaps

- `omp.telemetry.PromptFingerprint.slots` is only `Mapping[str, str]`, so per-slot byte sizes and stability bands are absent (`crates/py/python/omp/telemetry.py:108-121`; `docs/py/10-telemetry.md` §2 `PromptFingerprint`; stability catalog in `docs/py/08-context.md` §3). The overlay reports byte sizes as unavailable and labels only catalog-recognized bands.
- Frozen `omp.telemetry.ModelRequest` exposes only `seq`, `usage`, `prompt`, and `served_model`, and therefore lacks `degraded` (`crates/py/python/omp/telemetry.py:124-131`), diverging from `docs/py/10-telemetry.md` §2 `ModelRequest`, which specifies `tokens` and `degraded` among the full event fields. The overlay reports degradations as unavailable.
- `docs/py/10-telemetry.md` §4 says `Capture.CONTENT` adds `args_raw` and `Tokens.detail`, but its §2 `ModelRequest` has no `args_raw`, request-content, or response-content field, and the frozen `ModelRequest` has none either (`crates/py/python/omp/telemetry.py:124-131`). With the grant, only the frozen `Tokens.detail` field can be shown; without it, the explicit redaction notice is rendered.
