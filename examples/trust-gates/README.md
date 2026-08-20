## What the pi original did

`pi-sandbox` replaced Bash with a sandboxed implementation and independently gated network, read, and write access with configurable allowlists and interactive permission prompts (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`).

## The omp shape

This port isolates one narrow consequence of that policy surface: whether an extension may touch the user's ambient filesystem. `trust_read` reads `ctx.trust`, explicitly handles both frozen members, and widens only for `Trust.TRUSTED`. The trusted arm may open an ambient path and returns a bounded text preview; ordinary open failures are returned as data. The `Trust.SANDBOXED` arm never attempts the open and returns a successful broker-only result directing callers toward a declared `omp.env` document scope. An unknown future tier also fails soft and performs no ambient access (`docs/py/00-overview.md` §“Trust tiers” and §`omp.Context`).

The original replacement shell, allowlist evaluator, permission prompts, and extension-owned sandbox are deleted. Confinement is a property of the host child process rather than a Python object: sandboxed filesystem access is brokered through `omp.env`, while a trusted child may use the user's ambient access. The tier is conferred by the install record, never claimed in `omp.toml`. The frozen enum contains `SANDBOXED` and `TRUSTED` at `crates/py/python/omp/_scope.py:13-18`, and the callback accessor is frozen on `Context.trust` at `crates/py/python/omp/_context.py:33-46,61-78`.

## Gaps

None — every symbol this port needs is frozen.
