# Verdict details

## What the pi original did

`@eleboucher/pi-memini` provided shared cross-session memory, automatic briefings and recall, lifecycle capture and digests, and explicit memory-management tools. Pi kept full memory payloads **in model context and session history** so that message and tool renderers could separately show bounded one-line transcript views.

## The omp shape

This focused port uses an `analyze` device to demonstrate the split that pi was missing. Its returned `AnalyzePayload` is the full typed durable truth: it retains the complete source text, every term count, and all aggregate measurements. The host journals that verdict and applies the central spill gate above 4 KiB; the extension never truncates details, creates a temporary artifact, or owns a second persistence path (`docs/py/02-verdicts.md` §“One call, one truth, three projections” and §“The spill gate”).

The other two channels are pure views of that same verdict. `Analyze.prompt()` emits one terse line through `omp.Budget`, choosing a fuller or compact set of key numbers according to `PromptCaps.maximum_text_bytes`; replay can therefore project the same historical details at a different model budget without changing the stored truth (`docs/py/02-verdicts.md` §“Projecting for the model”). The `@omp.renderer` fold shows that one-liner when collapsed and builds the expanded source-and-term view directly from retained payload fields. It never parses model-visible parts and never re-analyzes the source (`docs/py/02-verdicts.md` §“Rendering: the update fold”).

## Gaps

None.
