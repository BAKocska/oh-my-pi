## What the pi original did

`pai-acp` provides active context pruning and model-driven message compression. The survey records that it cancels Pi's native compaction and rewrites context through Pi's mutable `context` event (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`, context-management cohort). This port isolates only the lossless stale-tool-result case; it does not claim to reproduce ACP's summarization.

## The omp shape

The mutable message-array rewrite and extension-owned compression path are deleted. A `thread_projection` domain hook instead examines immutable `MessageRef` metadata and returns bounded operations against stable item IDs. It starts eight percentage points below `ContextUsage.threshold_fraction`, computes the token headroom to recover from `usable_tokens`, protects the four newest turns, and emits at most sixteen operations. Only historical `TOOL_RESULT` items that are unpinned, not already elided, explicitly `useless`, non-empty, and at least 4 KiB are eligible.

Each intended `DropParts` operation names the whole tool-result item. Per `docs/py/02-verdicts.md` §4 and §“An amendment that drops a projection and keeps the verdict”, that operation removes model-visible `content` while retaining `details`, `error`, `useless`, and `omp/tool-rev`. Per `docs/py/08-context.md` §§“Two copies, one truth” and “Patch operations”, the hook edits only the per-request working projection: pinned items are excluded and the canonical journal is untouched. No part body is pulled, parsed, copied, summarized, or reconstructed.

This is the scheduling complement to `examples/output-budgets/`: that sibling declares bounded rendering and marks superseded payloads eligible with `Payload.useless()`, but deliberately leaves `DropParts` to Core. This port measures whether a frozen Python projection hook can request that exact lossless arm when context pressure actually arrives.

## Gaps

None — every symbol this port needs is frozen.
