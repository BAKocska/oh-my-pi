# Remote approval

## What the pi original did

`@agentapprove/pi` forwarded blocking tool approvals to Agent Approve on an iPhone or Apple Watch and mirrored agent activity there. Its unreachable-service setting could allow, deny, or fall back to Pi's local question instead of silently choosing one policy (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:182`).

## The omp shape

Core, not this extension, creates and durably journals one `ApprovalTicket` per invocation. `remote_approver` only offers that ticket, with `ticket_id` as the remote idempotency key; a re-offer first reads the remote state, so the same ticket is not forwarded twice. `extension_activate` upserts one Core-owned `Every` schedule and immediately reconciles pending tickets. Invisible schedule delivery wakes a turn, whose `turn_start` observer polls remote decisions and answers the existing ticket. There is no suspended gate coroutine, hand-rolled timer, local ticket ledger, or extension-painted approval dialog (docs/py/06-policy.md §“Approvals”, lines 1323–1377 and 1505–1531; docs/py/12-agents.md §“Scheduling”).

The webhook contract is `?action=status&ticket_id=…` and `?action=offer&ticket_id=…&payload=…`; the service must atomically retain `ticket_id` and return JSON with `state` equal to `missing`, `pending`, `allow`, or `deny`. The status lookup before each offer makes activation re-offers idempotent without treating extension memory as truth.

Unreachable behavior is intentionally conservative:

| `unreachable` | Result |
|---|---|
| `ask` | leave the Core ticket pending so `ESCALATE_LOCAL` can present the reserved local dialog |
| `deny` | answer with an `UNAVAILABLE` denial |
| `allow` | allow only when `allow_fail_open = true` **and** every reason is `ApprovalKind.READ`; otherwise deny |

The original's allow option is not the default because omp's fail-closed contract permits fail-open only for read-shaped work (`docs/py/06-policy.md`, §“omp.Unreachable”, lines 1487–1503). Requiring both explicit settings prevents a typo or inherited `unreachable = "allow"` from silently authorizing effects.

## Gaps

- `omp.approver` is documented at `docs/py/06-policy.md:1505-1526`, but the frozen top-level module defines only `device` and `tool` decorators at `crates/py/python/omp/__init__.py:596-762`; `approver` is also absent from `__all__` at `:798-1151`. The example therefore cannot register its first external approver on the frozen layer.
- Scheduled late resolution needs `omp.policy.decide(ticket_id, decision)` (or an equivalent answer verb), but frozen `crates/py/python/omp/policy.py:736-752` exposes only `pending()` and no answer operation. The documented decorator instead requires its coroutine to return one `ApprovalDecision` directly (`docs/py/06-policy.md:1505-1517`), which cannot express “offer now, return, decide on a later `Every` firing.”
- Frozen `omp.policy.pending()` is itself an explicit `NotWiredError` stub at `crates/py/python/omp/policy.py:736-738`, so activation reconciliation and scheduled polling cannot execute.
- Frozen HTTP exposes only `omp.env.http_get`, and that method raises `NotWiredError` at `crates/py/python/omp/env.py:905-913`; no POST/PUT webhook verb exists. `docs/py/11-env.md:2205-2213` records the v1 ruling to ship no Environment HTTP client. This example uses the only frozen spelling and a GET-shaped idempotent protocol to make the missing transport exact rather than opening a raw socket.
- `Every` has no callback delivery: frozen `Inject` carries only `mode` and `visible`, while `Delivery` is only `Inject | Spawn` (`crates/py/python/omp/agents.py:818-833`). The invisible-injection/`turn_start` bridge is the narrow available shape, but both `schedule()` and its handle methods are still `NotWiredError` stubs at `crates/py/python/omp/agents.py:887-950`.
