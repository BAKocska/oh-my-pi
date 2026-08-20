## What the pi original did

`pi-canary` inserted fixed or varying hidden tokens at selected conversation positions, replaced each user question with a hidden verification turn, stripped that exchange, and then triggered a second turn for the real answer. Its fixed-token/end-position mode was intended to preserve a local provider's KV cache.

## The omp shape

This port deletes message-array rewriting, hidden extra inference, phase flags, hand-triggered turns, configuration files, and automatic compaction. A pure `@omp.prompt_slot("guidance", cls=omp.SlotClass.STABLE)` instead carries one session-seeded canary sentence on every prompt. The sentence's bytes are deterministic for a session and never contain a turn number, clock value, random value, check result, or other per-turn text. Declaring it `VOLATILE` would place it below the stable cache breakpoint and permit a new contribution every turn, defeating the KV-friendly claim even if the text happened not to change (`docs/py/08-context.md` §“Prompt slots and prefix stability” and §`@omp.prompt_slot`).

A caller opts a turn into verification with the exact `[canary-check]` marker. The stable sentence tells the model to begin that answer with the session echo. A `before_agent_start` OBSERVE hook arms only the next committed turn, and the `turn_end` OBSERVE hook checks the bounded assistant `MessageRef.preview` values belonging to that turn; it never copies or reconstructs the provider message array (`docs/py/05-hooks.md` §3.11 family B; `docs/py/08-context.md` §`omp.MessageRef`). Every requested check increments `omp.ext.examples.canary.checks`; failures also increment `omp.ext.examples.canary.failures` and append one idempotent, typed `CanaryCheck` entry without retaining the token. `canary_status` folds recent failure entries from the journal, which remains the only durable truth (`docs/py/09-journal.md` §`omp.journal`; `docs/py/10-telemetry.md` §`counter(name, *, unit, description) -> Counter`).

## Gaps

None. The port uses the frozen `PromptContext`, `SlotClass.STABLE`, turn lifecycle payloads, context projection previews, extension counters, typed journal entries, and soft-device signatures without a frozen-versus-docs divergence.
