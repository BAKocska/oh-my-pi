## What the pi original did

`@ryan_nookpi/pi-extension-headroom` sent large tool results through a local Headroom compression proxy so their rewritten text occupied less of pi's mutable message array. The proxy was a second content-processing path whose availability, output format, and compression decisions became part of every later turn.

## The omp shape

The local compression proxy is deleted. omp's central spill already gives oversized output a typed `artifact://` address, so this port never recompresses or copies the body. Under pressure it replaces a giant old `TOOL_RESULT` with one model-facing line that names the existing `omp.ArtifactUrl`; output without an artifact is left intact rather than hidden behind an unrecoverable summary. Superseded results old enough to be safely distant are pruned losslessly with the default call placeholder. Every `Prune` and `Replace` names exactly one `MessageRef.id`, and `pinned` refs are excluded before an operation is built.

The trigger and stopping target come from `ContextUsage`: the handler begins at ten percentage points below the user's `threshold_fraction`, computes the token headroom to restore from `usable_tokens`, and stops when that budget is met. It emits at most eight operations below the configured threshold or sixteen at/above it, so both work and wire size are bounded. The later `superseded_by` id is checked against the same `ContextView`, giant replacements require an artifact, and prune/replace targets cannot overlap.

This is a `thread_projection` patch over the per-turn working copy, not a rewrite of durable journal truth (docs/py/08-context.md §§“Two copies, one truth”, `omp.MessageRef`, `omp.ContextUsage`, and “Patch operations”). The amended D1 invariant is: **Extensions may not replace or reserialize the provider message array. They may return bounded, validated projection operations against stable item IDs.**

## Gaps

None.
