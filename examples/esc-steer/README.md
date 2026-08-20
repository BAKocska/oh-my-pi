## What the pi original did

`pi-esc-steer` changed Esc while an agent was running: instead of merely pausing or dumping every queued message, it aborted the current turn and immediately continued with the next queued steer or follow-up (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`, `pi-esc-steer`).

## The omp shape

`/queue <text>` appends a typed `QueueTransition` to SESSION-scoped `omp.state`. Folding those transitions reconstructs one durable FIFO without a second state file. `ctrl+alt+escape` reads its head, first calls the sanctioned `omp.agents.inject("", mode=DeliveryMode.STEER)` loop verb to request an immediate steering interrupt, and only after that request completes injects the head with `DeliveryMode.NEXT_TURN`. A successful delivery appends the dequeue transition. With no queued item, the first call remains a plain abort and no follow-up is injected (`docs/py/12-agents.md` “Autonomous loops” and “Inter-session messaging”; `docs/py/09-journal.md` §`omp.state`; `docs/py/07-ui.md` §`@omp.shortcut`).

Extensions do not construct `omp.Interrupt` or choose its wire class. The loop is the only producer of interrupt classes; extensions select the frozen `DeliveryMode.STEER`, which Core maps to its immediate interrupt class (`docs/py/03-params.md:360-382`; `docs/py/12-agents.md:1102-1112`). This deletes the original key-handler patch, pause state, and dump-the-queue branch: the port composes Core-owned abort with one ordinary mailbox injection.

## Gaps

None.
