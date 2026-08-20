## What the pi original did

`@mrclrchtr/supi-context` monitored context-window pressure and token usage, exposed snapshots, and used `appendEntry` to put visual attribution reports in the transcript without putting those reports in the LLM context (`catalog.md:145`).

## The omp shape

`/context` reads the frozen `omp.context.usage()` result and copies every `ContextUsage` field into a typed `ContextReport`; it does not retokenize, sum, divide, or otherwise reconstruct usage. The latest assembler-owned `PromptFingerprint` contributes each slot's own `size_bytes`, stability `band`, and digest, rather than guessing from slot names (`docs/py/08-context.md` §`omp.ContextUsage`; `docs/py/10-telemetry.md` §`class PromptFingerprint`).

`ContextReport` defines `render` but deliberately has no `project`. It is therefore a displayed typed journal entry, not a thread item or a model-facing `Part`: the report is visible in the transcript and contributes zero model-context parts (`docs/py/09-journal.md` §Versioning and §`@omp.entry_kind`, especially the render-without-project rule at lines 238-244 and example conclusion at lines 289-291). The `pressure` telemetry subscription retains the latest assembler fingerprint and updates one coalesced status contribution; severity compares the host-owned `fraction` directly with the host-owned `threshold_fraction`.

Deleted mechanisms: Pi custom entries, extension-side token counting, prompt hashing, slot-band inference, transcript text that leaks into the model, and hand-managed status refresh hooks. Catalog notifications remain message-list items: their tokens are already included in `message_tokens`, while the report states the separate-echo caveat rather than manufacturing a number (`docs/py/08-context.md:2091-2102`).

## Gaps

- `ContextUsage.catalog_notice_tokens` is absent from the frozen `crates/py/python/omp/context.py:91-106`, although the ruled accounting split requires notification tokens to be counted in `message_tokens` and echoed separately; `docs/py/08-context.md:2091-2102` still labels that split unresolved while describing the same proposed field. The example reports the truthful location and caveat but cannot display the separate echo without recomputing it.
