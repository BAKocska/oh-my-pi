## What the pi original did

`pi-hermes-memory` kept user preferences, failure lessons, procedures, and searchable session history in SQLite FTS5 databases alongside mutable `MEMORY.md`, `USER.md`, and `STANDING.md` files. It also backfilled the database by parsing Pi's private session JSONL files directly. Its good default was policy-only prompt injection rather than dumping the whole memory bank into every turn.

## The omp shape

This port makes typed `MemoryEntry` records in the project-scoped `omp.state` log the durable truth; `memory.db` under `await omp.state_dir()` is only an FTS artifact and is repopulated by replay. Historical conversation text comes through `omp.sessions.list()` and `omp.sessions.journal(..., kinds=("omp.message",))`, never from transcript files. The soft `memory_search` device is declared with `place="env"`, so SQLite scans happen beside the state directory, and the model reaches it through `dyn` with `{"do_": "docs/memory_search"}` and `{"do_": "invoke/memory_search", ...}` rather than a URL scheme (docs/py/08 §3 and docs/py/09 §4).

Prompt text is split into exactly two semantic bands: `guidance` holds the STABLE memory policy plus profile and standing snapshots, while `recall` holds only the VOLATILE, query-dependent snippet. The original appended both stable policy/profile material and changing recall text to one system-prompt string; every recall change therefore invalidated the unchanged prefix, which was the cache-churn bug (docs/py/08 §3, “split across the boundary that matters”).

## Gaps

- There is no frozen-vs-docs signature divergence for the frozen APIs used here: `omp.device` includes its required `family` and integer `rev`, `omp.state` calls are awaited with `scope=omp.StateScope.PROJECT`, and `omp.state_dir()` is awaited before calling `EnvPath.local_path()` inside the env-placed body.
