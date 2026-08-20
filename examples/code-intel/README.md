## What the pi original did

`@mrclrchtr/supi-code-intelligence` managed TypeScript, Rust, Python, and Go language-server subprocesses itself, including initialization, document synchronization, restart handling, and session-scoped reference counts. It also supplied navigation, diagnostics, symbols, and batch analysis, writing large reports to private temporary files.

## The omp shape

The three focused queries open one Environment document lease and call the bound server through `omp.env.lsp.bindings()` and `omp.env.lsp.request()`; there is no process, watcher, synchronization, or restart code because the Environment owns that lifecycle (`docs/py/11-env.md` §1). `survey` is placed at `omp.Place.ENV`, awaits `omp.workers.get()`, then uses the asynchronous, intentionally serial `WorkerHandle.map()` surface. Oversized aggregates return `omp.Spill` instead of inventing a temporary path (`docs/py/04-placement.md` §3).

## Gaps

- No symbol gap remains for the LSP surface used here: the frozen layer exports `omp.env.lsp.bindings()` and `omp.env.lsp.request()`, and the Round 8 remediation froze the documented `LspBinding`, `LspStale`, and `LspFailure` value types plus `lsp.last_revision`. The binding receipt is still handled structurally here, and the request uses the documented default stale policy.
- `omp.env.lsp.request()` accepts the opaque server handle returned in a binding receipt; the port passes it through without assuming a `str` or `bytes` representation.
- `Doc.uri` and `Doc.revision` are frozen. `Doc.dry_run` remains pending.
- `WorkerHandle.map()` is intentionally serial until the Part 3 supervisor lands; `concurrency=` is validated and reserved for that backing.
- `workers.get()` is asynchronous, matching the CONTROL-round-trip contract. The remote decorator ships in the standalone `omp_remote` module (Round 8 ruling: it is deliberately importable without the `omp` package; the docs' former `omp.remote` spelling was corrected).
- Frozen `omp.Spill` carries `value: bytes` plus `media_type=` (Round 8 closed the former divergence), so the spilled aggregate presents as its real media type.
