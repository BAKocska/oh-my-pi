# Vector memory

## What the pi original did

[`@galvinsan/pi-mentis`](../../.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md#L96) used `@zvec/zvec` for vector retrieval, document indexing, memory commit/search, and automatic evidence injection. [`pi-mentis-memory`](../../.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md#L98) added provenance-aware long-term memory and relationship reasoning over recalled items. Both kept a native vector store in the extension process.

## The omp shape

This worked port keeps the two useful device boundaries and deletes the extension-owned subprocess protocol. `memory_put` first appends a typed `MemoryRecorded` journal entry; `memory_query` reads the same durable ledger. Both host-side device entry points ship only embedding/index bodies and plain data to the manifest-declared `vectors` worker. The worker owns the hash-based stub embedder, SQLite vector store, and every native-adjacent import. In a production port, a native embedding or vector-store wheel is imported by `_boot_vectors` in that same worker, never by the extension host.

The worker is declared as `place="worker:vectors"` with `site="env"`, so it is a distinct supervised process beside `await omp.state_dir()`. That placement is required because `EnvPath.local_path()` is legal only beside the Environment. It still gives native-crash isolation: `restart="on-failure"` advances the worker generation and respawns it without killing the host or session. This is the local, state-directory-bearing variant of [placement §4, “Native-crash isolation”](../../docs/py/04-placement.md#4-native-crash-isolation--pi-onnx-and-vector-memory-extensions), combined with the env-colocated rebuild pattern in [`omp.state_dir`](../../docs/py/09-journal.md#ompstate_dir).

The durability equation is deliberately one-way:

```text
J' = J ⧺ [MemoryRecorded]
I  = F(J')
```

`J'` is durable journal truth; `I` is `vectors.db`, a replaceable index under `state_dir()`. If the worker crashes during `F`, the call's effects are unknown at the placement boundary, but `J'` is unchanged. The supervisor respawns generation `g + 1`, and activation or the next put/query replays the complete journal and assigns `I := F(J')` transactionally. The session never shares the worker's crash fate, and deleting `vectors.db` loses no memory.

The hash embedder is deterministic and downloads no model. It exists only to make put, query, crash recovery, and rebuild executable as an example; it is not presented as a semantic embedding model. The tools remain discoverable only through `dyn`.

## Gaps

None. The port uses the frozen signatures for `omp.entry_kind`, `omp.journal`, `omp.state_dir`, `omp.workers`, placement, hooks, and devices without a docs divergence.
