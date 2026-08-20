# Spec flow

## What the pi original did

`@mrclrchtr/supi-flow` coordinated a spec-driven `brainstorm → plan → apply → archive` workflow through seven custom tools, five auto-discovered skills, and TNDM ticket files.

## The omp shape

One soft `flow` device owns four subtools: `start`, `advance`, `status`, and `archive`. Typed `FlowTransition` entries are the phase machine; there is no state file. Every phase entry declares its model role and complete available-device set. A transition publishes one `omp.devices.set_availability(*deltas)` batch, while the request tool array remains byte-identical as required by `docs/py/01-devices.md` §“Availability is a notification, not a re-registration.”

`advance` and `archive` refuse to move unless given the completed prior-phase artifact, returning `MissingArtifactFault` with its exact phase, ticket, and expected path. Artifacts are committed and read only through revision-pinned `omp.env.docs` leases (`docs/py/11-env.md` §“Document leases — `omp.env.docs`”), never direct filesystem writes. Archive encodes the three-document bundle once, spills it through `omp.env.blobs.put`, then atomically journals both the `FlowArchive` blob reference and the terminal transition. Blob reachability therefore comes from journal truth (`docs/py/09-journal.md` §“Storage classes and their authority”).

The original seven-tool sprawl is deleted in favor of one device tree. The ticket remains a stable coordination identity inside typed journal entries and artifact paths; ticket-file polling, lock files, and a second phase store are deleted. This follows `docs/py/09-journal.md` §`omp.journal`: declared frozen dataclasses are durable truth, and `append_atomic` makes archive publication all-or-nothing.

## Gaps

- `omp.Device.subtool`: frozen `crates/py/python/omp/devices.py:362-423` accepts only `name`, hard-codes every child `schema=None`, and inherits all parent declaration properties. `docs/py/01-devices.md` §`omp.Device` (`:911-929`) first documents the name-only signature, then says a subtool call can override inherited properties and that every leaf carries its own schema. The frozen signature and the passage are contradictory; typed child bodies still dispatch through the landed decorator, but callers cannot express the documented overrides or inspect a child schema on its handle.
