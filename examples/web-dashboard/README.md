# Web dashboard

## What the pi original did

`@jmfederico/pi-web` exposed a slash command that managed a background Fastify web UI and persistent session daemon. The secondary origin, `@firstpick/pi-package-webui`, added a browser companion and bidirectional WebSocket RPC bridge. Both extensions owned daemon discovery and lifecycle themselves: select a loopback port, spawn a child, poll it until responsive, persist a PID or service record for later sessions, and run their own restart handling.

## The omp shape

The soft `dashboard` device queries the core-maintained index with `omp.sessions.list()` and `omp.sessions.usage()`, folds those durable rows into one bounded JSON snapshot, and sends a complete replacement as one JSON line to a workspace-named process. That environment-owned process runs a tiny bundled standard-library HTTP server and serves the latest value at `http://127.0.0.1:8765/api/dashboard`. The device returns that URL plus the supervised state and generation. Its launch command embeds the bundled server source and never derives a path from `__file__` or the host working directory.

The intended lifecycle is exactly `omp.env.proc.ensure`: adopt the existing `examples-web-dashboard` generation or atomically start it, require both the listening log and TCP port readiness, restart on failure with bounded backoff, and leave workspace-idle reclamation to the Environment (`docs/py/11-env.md` §3, “A supervised daemon,” and §“Named processes”). The port is a fixed declaration, not a port-selection algorithm; a bind conflict fails readiness visibly rather than causing the extension to hunt for another port and return an unstable address.

Deleted mechanisms:

- **Port picking:** no random scan, increment-until-free loop, or 3000/8080 fallback chain.
- **PID files:** the stable process name and `proc.ensure` are the authority; the extension never writes or trusts a PID.
- **Readiness poll loops:** log and TCP readiness are supervisor probes, so process creation is never mistaken for readiness.
- **Restart/watchdog loops:** the launch spec carries bounded on-failure restart policy.
- **Session-file crawling:** session and cost truth comes from `omp.sessions` indexed journal receipts, not JSONL files or a private mirror.
- **Shutdown guessing:** the device does not kill a workspace-owned daemon that another session may still use.

## Gaps

Gap closure should reuse the already-frozen top-level `omp.Restart` vocabulary from `crates/py/python/omp/placement.py:19-21` for both workers and named processes instead of creating a second Python enum. The readiness family should likewise track the protocol's `ReadyProbe` variants: log, TCP, and the ratified Ping/Pong probe (`crates/proto/proto/omp/env/v1/env.proto:274-297`; `docs/py/04-placement.md` §“Resolved,” lines 2585-2592). If combined `ReadyAll(log, tcp)` remains the Python contract required here, it also needs an honest wire representation: the current protocol field is a single `oneof`, despite `docs/py/11-env.md:1171-1176` claiming combined probes mirror that wire exactly.
