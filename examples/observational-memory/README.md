## What the pi original did

`pi-observational-memory` captured background observations and reflections, stored them as custom session entries, and intercepted session compaction to fold that ledger into a structured summary. Its compaction callback used a `compactHookInFlight` boolean to prevent re-entry, and its observation pipeline paid for model calls before the final deterministic fold.

## The omp shape

This port records every settled `tool_result` at `HookPhase.OBSERVE` as a typed `ObservationRecorded` journal entry. The LOCAL compaction hook folds those entries directly into `CustomSummary`, preserving any prior summary and returning `None` when the ledger is empty or when `suggested_first_kept` is not present in `to_summarize ∪ to_retain`. That explicit check mirrors the host validity rule: an unknown `first_kept_id` must never be submitted. No summarizer model is called; once the durable ledger already contains the structured facts, paying an LLM to restate them is pure waste (docs/py/08-context.md §2, “an incremental ledger that supplies its own summary”, and §Compaction control).

After `compaction_done`, the port calls `omp.prompts.invalidate("memory")` so the epochal prompt band is refreshed. At `agent_settled`, it requests `await omp.context.compact()` and tolerates `CompactionBusy`; this is the sanctioned out-of-band request and replaces the pi extension's `compactHookInFlight` process-local boolean. The journal remains the only truth—there are no side files or authoritative caches. Model dispatch, if later added for richer observations, would have to use attributed `omp.agents.completion`; this focused outcome ledger needs none.

## Gaps

- `omp.CompactionEvent`, `omp.CompactionOutcome`, `omp.CompactionTier`, `omp.CompactionVerdict`, and `omp.CustomSummary` are documented in `docs/py/08-context.md` §Compaction control and used by its §2 worked port, but are not defined/exported by `crates/py/python/omp/events.py` or `crates/py/python/omp/__init__.py`. The latter only wildcard-exports the types that `events.py` actually defines.
- `omp.context.compact()` and `omp.CompactionBusy` are documented in `docs/py/08-context.md` §“omp.ContextEpoch and omp.context”, but `crates/py/python/omp/_context.py` defines only the callback `Context` value and `crates/py/python/omp/__init__.py` exports no `context` namespace or `CompactionBusy`.
- The documented `@omp.hook("compaction")` domain-return form in `docs/py/08-context.md` §2 cannot register: `crates/py/python/omp/hooks.py` `_DOMAIN_EVENTS` omits `"compaction"`, so `hook()` raises `HookContractError` unless a phase is supplied, while the documented verdict is not a `HookDecision`. The source intentionally retains the documented form rather than disguising the frozen-layer defect.
