# Recall

## What the pi original did

[`@joshbochu/pi-recall`](https://www.npmjs.com/package/@joshbochu/pi-recall) built a native full-text index over rendered Pi session prose for fast search and resume (`catalog.md:272`).

## The omp shape

The soft `recall` device queries settled `tool_call` outcomes through the structured telemetry query described by `docs/py/10-telemetry.md` §query. Every request names one exact canonical revision such as `edit@hl.3`, has a bounded lookback, and pushes equality predicates on typed fields such as `outcome`, `fault_code`, `abort.kind`, or `payload.*` into Core. It never searches model-facing result prose. Rows are folded by `(tool, rev)` and rendered as a bounded table; a spilled outcome remains an `artifact://…` reference rather than being copied into the table. This is the structured-verdict replacement required by `docs/py/02-verdicts.md:1459-1461` and uses journal-backed telemetry as truth.

The deleted mechanism is the original prose-FTS session index. Supplying `terms` enables a rebuildable SQLite FTS cache at `await omp.state_dir()` only after the authoritative typed query. That cache contains typed scalar field paths and values, is stamped `truth=false`, is rebuilt from the current query result, and only performs a secondary filter. It is an index, never truth; stale or deleted cache files cannot change the durable outcome record.

## Gaps

- `omp.telemetry.Predicate`, `Eq`, `Step`, `Query`, `query`, `QueryResult`, and `Row` are specified by `docs/py/10-telemetry.md:1145-1240` but absent from the frozen `crates/py/python/omp/telemetry.py`; its public surface ends at `crates/py/python/omp/telemetry.py:451-464`. The port isolates this missing CONTROL call in `_query_outcomes`. Closure must also preserve the resolved predicate semantics at `docs/py/10-telemetry.md:2540-2544`: `where` compiles to SQL for index pushdown, while the Rust backfill evaluator remains behaviorally identical through a shared conformance corpus, rather than introducing a Python-side evaluator.
