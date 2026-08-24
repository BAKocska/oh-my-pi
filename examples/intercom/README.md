## What the pi original did

`pi-intercom` enabled direct one-to-one coordination between agent sessions on the same machine. It started a detached out-of-process broker, addressed a Unix socket under `~/.pi/intercom/`, framed JSON messages with a hand-written length prefix, and tracked broker ownership with a PID file so blocking and non-blocking clients could find it.

## The omp shape

The soft `intercom` device is reached through the core shell as `xd intercom …`. Its `send` operation accepts addressable agent or `session:<ulid>/Agent` destinations and returns one core receipt per destination; `inbox` drains canonical peer messages; `wait` delegates to `omp.agents.wait_for`, whose roster liveness check returns `None` when the awaited peer terminates instead of deadlocking. Peer messages are canonical thread items and therefore land in the session journal without an extension-owned message store. The soft `peers` device (`xd peers …`) folds the core roster into compact addressable rows. Exact flags are scriptable and discoverable with `xd intercom --help` and `xd peers --help`. These semantics are specified by `docs/py/12-agents.md` §Inter-session messaging and its broker implementation notes.

`@omp.service("examples.intercom.notify", rev=1)` exposes the same non-blocking fan-out to sibling extensions without turning journal entries into RPC. A consuming extension declares `services = ["examples.intercom.notify@1"]` under its manifest `[requires]`, connects with `await omp.services.connect("examples.intercom.notify", rev=1)`, and calls the typed `notify(NotifyRequest(...))` method. The provider declaration is the manifest `[[services]]` row. Calls use the correlated CONTROL service path and its grant check, as required by `docs/py/00-overview.md` §Extension services and §`@omp.service` / `omp.services`.

The detached broker daemon is deleted because routing is Core-owned and project-scoped. The broker socket path is deleted because extensions use CONTROL. Length-prefixed framing is deleted because no extension-owned transport remains. The PID file is deleted because there is no extension-owned process to discover or reap. The original identity environment variables and heartbeat loop disappear for the same reason. This is the ownership correction described by `docs/py/00-overview.md` §3 `pi-intercom` and `docs/py/12-agents.md` §5 `pi-intercom`.

## Gaps

None — every symbol this port needs is frozen.
