## What the pi original did

`@mrclrchtr/supi-code-intelligence` managed TypeScript, Rust, Python, and Go language-server subprocesses itself, including initialization, document synchronization, restart handling, and session-scoped reference counts. It also supplied navigation, diagnostics, symbols, and batch analysis, writing large reports to private temporary files.

## The omp shape

The three focused queries open one Environment document lease and call the bound server through `omp.env.lsp.bindings()` and `omp.env.lsp.request()`; there is no process, watcher, synchronization, or restart code because the Environment owns that lifecycle (`docs/py/11-env.md` §1). `survey` is placed at `omp.Place.ENV`, awaits `omp.workers.get()`, then uses the asynchronous, intentionally serial `WorkerHandle.map()` surface. Oversized aggregates return `omp.Spill` instead of inventing a temporary path (`docs/py/04-placement.md` §3).

## Gaps

- No symbol gap exists for the LSP calls used here: the frozen layer exports `omp.env.lsp.bindings()` and `omp.env.lsp.request()`. It does not yet export the documented `LspBinding`, `LspStale`, or `LspFailure` value types, nor `lsp.last_revision`; consequently the binding receipt is handled structurally and the request uses the documented default stale policy.
- `omp.env.lsp.request()` accepts the opaque server handle returned in a binding receipt; the port passes it through without assuming a `str` or `bytes` representation.
- `Doc.uri` and `Doc.revision` are frozen. `Doc.dry_run` remains pending.
- `WorkerHandle.map()` is intentionally serial until the Part 3 supervisor lands; `concurrency=` is validated and reserved for that backing.
- `workers.get()` is asynchronous, matching the CONTROL-round-trip contract. The documented `omp.remote` decorator remains unfrozen.
- Frozen `omp.Spill` accepts only `value: bytes`, unlike the docs' `media_type=` example. The spill therefore has no extension-supplied media type until that signature lands.
