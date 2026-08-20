# Feature bundle

## What the pi original did

`pi-toolbox` shipped 17 extensions and `@bdsqqq/pi` registered 33 separate entrypoints. Each entrypoint was independently loaded even though the package was one umbrella product.

## The omp shape

This port is one extension with three enable units: `greet`, `stamp`, and `badge`. Each `[features.*]` table has its own entry module, default, description, and feature-scoped dependency list. The root package deliberately imports none of them, so a disabled feature materializes no decorator declarations and its `requires` do not enter resolution.

The granularity rule is “distribution unit = package, enable unit = feature” ([14-deploy.md §3.2.1](../../docs/py/14-deploy.md#321-umbrella-bundles-are-the-norm-not-the-exception)); feature tables are defined by [§3.1.3](../../docs/py/14-deploy.md#313-packaging-tables-owned-here). The 33 pi entrypoints therefore collapse to one `omp.toml`, one process, one site tree, and one consent diff rather than 33 copies of each. This follows the per-extension host boundary in [§2.2](../../docs/py/14-deploy.md#22-hosts-are-keyed-by-layer-tier-extension).

The old entrypoint array and per-entry loader are deleted. Static `[[tools]]` and `[[hooks]]` rows point at feature modules; only enabled feature entries are imported. `stamp` is an OBSERVE hook and writes a declared typed entry as required by [05-hooks.md §3.4](../../docs/py/05-hooks.md#34-the-five-phases) and [09-journal.md `omp.journal.append`](../../docs/py/09-journal.md#ompjournalappendentry--displaynone-idempotency_keynone---ompentryid). `badge` uses the retained status API from [07-ui.md §5.1](../../docs/py/07-ui.md#51-powerline-statusline--footer--widgets--hotkeys).

## Gaps

- `omp.Context.session`: the frozen surface is `str` at `crates/py/python/omp/_context.py:37`, agreeing with `docs/py/00-overview.md` §Context line 628, but `docs/py/07-ui.md` §5.1 lines 1844 and 1915 documents `ctx.session.stats`. The UI recipe contradicts the frozen signature and the overview.
