## What this probes

There is no pi origin for this extension. It is a conformance probe for the request quartet and the state-owner rules that make crash replay safe: a retry changes `request_id` but keeps `idempotency_key`, while a host or session generation mismatch is rejected before durable state changes. The owner stub records terminal outcomes by `(operation, idempotency_key)`, keeps process and worker generations, and keeps document heads separately from lease pins. The normative contract is in `docs/py/00-overview.md:353-364` (§“Idempotency and generation fencing”), `docs/py/09-journal.md:161-169` (durable-state rule 7), and PLAN §D8 (`PLAN.md:185-197`).

The executable smoke is `fencing_probe.py`. On 2026-08-20 it was re-run with the native stubs, frozen Python layer, and installed CONTROL stub. All 20 rows conformed. The four durable operations receive the complete quartet explicitly; replay attempts use a new `request_id` and the same `idempotency_key`. Process, worker, document, and transaction rows pass through the frozen SDK adapters. Worker observations additionally prove that `WorkerHandle.call` forwards its held generation, preserves `StaleGeneration`, and refuses draining or evicted workers before dispatch. The placement taxonomy row proves every placement exception is under the exported native `omp.PlacementError`.

| Operation | Replay or stale condition | Re-observed outcome | Status |
|---|---|---|---|
| `journal.append_atomic` | same idempotency key, new request id | recorded result returned; apply count remains one | conformant in state-owner stub |
| `journal.append_atomic` | stale `host_generation` | `StaleGeneration`; apply count unchanged | conformant in state-owner stub |
| `journal.append_atomic` | stale `session_generation` | `StaleGeneration`; apply count unchanged | conformant in state-owner stub |
| schedules upsert | same idempotency key, new request id | recorded result returned; apply count remains one | conformant in state-owner stub |
| schedules upsert | stale `host_generation` | `StaleGeneration`; apply count unchanged | conformant in state-owner stub |
| schedules upsert | stale `session_generation` | `StaleGeneration`; apply count unchanged | conformant in state-owner stub |
| `artifacts.adopt` | same idempotency key, new request id | recorded result returned; apply count remains one | conformant in state-owner stub |
| `artifacts.adopt` | stale `host_generation` | `StaleGeneration`; apply count unchanged | conformant in state-owner stub |
| `artifacts.adopt` | stale `session_generation` | `StaleGeneration`; apply count unchanged | conformant in state-owner stub |
| approval decide | same idempotency key, new request id | recorded result returned; apply count remains one | conformant in state-owner stub |
| approval decide | stale `host_generation` | `StaleGeneration`; apply count unchanged | conformant in state-owner stub |
| approval decide | stale `session_generation` | `StaleGeneration`; apply count unchanged | conformant in state-owner stub |
| `journal.append_many` | third append fails after two durable appends | `JournalError.appended=[2, 3]` preserves exactly the landed prefix | conformant in state-owner stub |
| `Process.send` | generation-1 handle, current named process is generation 2 | `PreconditionFailed`; no send | conformant through frozen adapter |
| `WorkerHandle.call` | generation-1 handle, current named worker is generation 2 | generation 1 forwarded; native `StaleGeneration` preserved; no call applied | conformant through frozen adapter |
| `WorkerHandle.call` | worker is `DRAINING` | `WorkerEvicted`; call not dispatched | conformant through frozen adapter |
| `WorkerHandle.call` | worker is `EVICTED` | `WorkerEvicted`; call not dispatched | conformant through frozen adapter |
| placement errors | placement subclasses and invalid `Place.parse` input | one native `omp.PlacementError` hierarchy | conformant through frozen adapter |
| `Doc.write` | lease pin is revision 1, document head is revision 2 | `Conflict(expected=rev1, current=rev2)`; no write | conformant through frozen adapter |
| `Txn.commit` | operation 2 fails after operation 1 is durable | distinct `Partial(committed=1, failed_index=1)`, not `Conflict` | conformant through frozen adapter |

## Closure records

1. **Worker generation forwarding and exception preservation — closed.** The defect was that `WorkerHandle.call` omitted `self.generation`, after which `_Workers._call` converted the state owner's `StaleGeneration` into `WorkerUnavailable`. The fix is at `crates/py/python/omp/placement.py:157-167,265-287`: the handle forwards its generation and `_call` preserves `StaleGeneration`. Re-observation recorded `generation=1` at the transport, received `StaleGeneration` unchanged, and observed no apply.

2. **Draining and evicted worker admission — closed.** The defect was the absence of a distinct lifecycle refusal and a pre-dispatch gate for terminal/draining generations. `WorkerEvicted` and the call gate are implemented at `crates/py/python/omp/placement.py:28-29,157-166`. Re-observation put separate handles in `DRAINING` and `EVICTED`; both raised `WorkerEvicted` and neither reached `worker_op`.

3. **Split placement-error hierarchy — closed.** The defect was that placement subclasses derived from a private Python `PlacementError`, so `except omp.PlacementError` did not catch them. The fix imports the native class and derives all placement failures from it at `crates/py/python/omp/placement.py:9,26-33`; the same native class is exported at `crates/py/python/omp/__init__.py:43,1500`. Re-observation proved `WorkerUnavailable`, `WorkerEvicted`, `ShipError`, and `BoundaryError` are subclasses of `omp.PlacementError`, and invalid `Place.parse` input raises that native class.

4. **Process, document-revision, and partial-commit fencing — reconfirmed closed.** `Process.send` forwards name and generation at `crates/py/python/omp/env.py:1100-1107`; `Doc.write` forwards its server-owned lease at `crates/py/python/omp/env.py:524-526`; and `Conflict` and `Partial` retain distinct carriers at `crates/py/python/omp/env.py:98-113,161-174`. Re-observation produced `PreconditionFailed`, `Conflict`, and `Partial(committed=1, failed_index=1)` respectively, with no conflation or unintended mutation.

## Sanctioned pending host arms — not findings

The earlier report treated unavailable host dispatch and transport-envelope fields as frozen-layer defects. That was incorrect. `PLAN.md` Part 4 owns the CONTROL dispatcher, and `NotWiredError` is the sanctioned state for a frozen symbol whose host arm has not landed. The authenticated transport, not every public function signature, owns `request_id`, generation stamps, and any operation-specific derivation of retry identity. The executable state-owner rows above remain the conformance evidence for admission, replay, and mutation ordering.

- `omp.journal.append`, `append_many`, and `append_atomic` expose their documented caller keys but remain intentionally unwired at `crates/py/python/omp/journal.py:223-251`.
- `omp.agents.schedule` remains intentionally unwired at `crates/py/python/omp/agents.py:938-955`; absence of quartet fields from its public domain signature is not evidence that the eventual authenticated request envelope omits them.
- `omp.artifacts.adopt` reaches CONTROL through `_request` at `crates/py/python/omp/artifacts.py:138-157`; transport-owned fencing fields are deliberately absent from its domain signature.
- `omp.policy.decide` remains intentionally unwired at `crates/py/python/omp/policy.py:788-792`; transport-owned fencing fields are deliberately absent from its domain signature.

No frozen-layer fencing finding remains open after this re-observation.
