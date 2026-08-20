# Developer inspector

## What the pi original did

`pi-dev-inspector` exposed the assembled system prompt and captured provider API request/response round-trips for interactive debugging.

## The omp shape

`/inspect prompt` opens a read-only overlay over the latest assembler-owned `PromptFingerprint`: slot keys and changed flags come directly from the fingerprint, and the implementation never hashes prompt material. Stability labels follow the fixed prompt slot catalog in `docs/py/08-context.md` §3; unknown assembler keys remain explicitly unknown rather than guessed. `/inspect request` queries recent `ModelRequest` rows for only the active session and shows the served model, total tokens, and cache hit rate (`docs/py/10-telemetry.md` §2).

The request overlay checks the invocation's actual `telemetry.capture_content` grant before rendering `Tokens.detail`. Without it, the overlay always renders an explicit redaction notice. Capture is a grant, not a default: `Capture.CONTENT` additionally requires an explicit durable user grant and is never implied by trust tier (`docs/py/10-telemetry.md` §4). The port is read-only: it registers no hooks, telemetry sinks, journal entries, devices, or environment effects.

## Gaps

None — every symbol this port needs is frozen.
