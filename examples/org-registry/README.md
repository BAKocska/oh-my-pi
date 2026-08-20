## What the pi original did

`@7n/rules` synchronized opinionated coding rules and skills into repositories and checked projects for conformity (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`). This derived port isolates the small shared-registry part of that shape: a team can pin named conventions once and let projects read the effective catalog.

## The omp shape

The repository config file, home-directory catalog, synchronization pass, lock file, and path-based organization guessing are deleted. `org_registry` journals typed `ConventionChange` entries directly through `omp.state`: `set` and `remove` prefer `StateScope.ORGANIZATION`, while `get` and `list` rebuild the effective registry from durable scoped entries. Organization values are folded first and active `StateScope.USER` fallback values overlay them, so a denied write remains readable even when organization reads are permitted. After a later organization write succeeds, a user-scoped reset marker clears any stale fallback for that name. There is no process-local registry or second source of truth (`docs/py/09-journal.md` §`omp.state`, especially lines 535–603).

Organization access is deliberately attempted rather than inferred from settings. The documented scope contract makes `ORGANIZATION` org-distributed and requires an org-level grant for writes; an operation disallowed by the manifest or org policy raises `omp.StateScopeDenied` (`docs/py/09-journal.md` lines 519–520, 535–559). Both paths are explicit here: an organization read denial falls back to the authenticated principal's durable `USER` registry, and an organization append denial journals the same mutation at `USER` scope before returning `write_scope="user"`. Other journal failures remain fail-closed and are not mistaken for authorization denials. Core, not this extension, chooses and stamps the authenticated `ctx.principal`; USER scope spans that principal's projects on one daemon, while durable authorship carries the principal and extension provenance (`docs/py/00-overview.md` §“Principal identity”, lines 338–351; `docs/py/09-journal.md` lines 540–559).

The frozen surface used without adapters is `StateScope` (`crates/py/python/omp/__init__.py:22-52`), concrete `StateScopeDenied` (`:171-172`), and the awaited `state.append`/`state.entries` CONTROL requests (`:216-242`). The host-placed soft device performs no work at import time beyond registering its device and entry-kind declarations.

## Gaps

None — every symbol this port needs is frozen.
