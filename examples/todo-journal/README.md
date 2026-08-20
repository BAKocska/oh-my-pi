## What the pi original did

`@xaccefy/pi-xtodo` kept a model-callable todo list, a status widget, and an interactive command overlay. It maintained the same list in an LRU-bounded memory map, a JSON file per session, and reconstructed tool results, then reconciled those copies by choosing the one with the highest `nextId`. That approach could become stale after compaction and could lose concurrent updates.

## The omp shape

This port keeps only one soft `todo` device and one renderer. Following `docs/py/09-journal.md` §2, each add, done, or drop is a typed `TodoItem` journal entry; the current list is rebuilt from `journal.entries(TodoItem)` on every device call, and batches use `append_many` with the caller's idempotency key. There is no cache, reconciliation routine, state file, status widget, or command overlay: the journal's core-assigned indexes and total order are what the pi original's three-way reconciliation was trying to fake.

The Python declarations are now frozen, but durable CONTROL backing remains pending: the Track B acceptance is a real add/done/drop round trip with idempotent replay, not an in-process substitute.

## Gaps

- Frozen `omp.device` accepts only `name`, `family`, `rev`, and `place`, unlike the broader documented signature in `docs/py/01-devices.md`; this example deliberately uses only the frozen signature. Frozen `omp.renderer` already matches the documented `(name, family, rev, reduce)` registration signature.
