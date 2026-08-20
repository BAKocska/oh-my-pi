## What the pi original did

`@braintrust/pi-extension` derived a platform-specific socket path, detached and `unref`'d `bt trace daemon`, and implemented newline-delimited JSON-RPC request IDs, timeouts, reconnects, and shutdown flushing while forwarding untyped lifecycle payloads.

## The omp shape

Following `docs/py/10-telemetry.md` §1 and §export targets, this port declares one replaying `@omp.telemetry` subscription for turn, tool, and model kinds. Replay is host-owned snapshot-at-watermark → oldest-first chronological delivery → atomic live delivery; escaped sink failures are host-owned fail-open `sink_error` warnings with no retry. `trace_frame` demonstrates typed event-to-frame mapping, while `ProcessTarget` names an Environment-managed process and sends one `handshake` frame; settings can instead select direct `OtlpTarget` egress. There is no `spawn()`, socket-path derivation, socket handling, JSON-RPC framing, retry loop, or side file. The `EVENTS_SEEN` instrument is declared through `omp.telemetry.counter`, whose namespace is reserved under `omp.ext.<id>`.

## Gaps

- `omp.telemetry.ProcessTarget`, `omp.telemetry.OtlpTarget`, and `omp.telemetry.export` are documented in `docs/py/10-telemetry.md` §export targets (lines 1357–1412) but are absent from the frozen `crates/py/python/omp/telemetry.py` exports (`__all__`, lines 258–262). The documented imports are retained with a GAP marker, so compilation succeeds but activation cannot import until the frozen layer lands them.
- Extension counter qualification diverges from `docs/py/10-telemetry.md` §Semconv (lines 159–165): frozen `crates/py/python/omp/telemetry.py::_instrument_name` (lines 216–219) emits the literal placeholder `omp.ext.<extension>.<name>` rather than `omp.ext.<id>.<name>`, and `Counter.add` (lines 188–192) is not wired. `EVENTS_SEEN` therefore states the intended declaration but cannot yet record the required `omp.ext.examples.trace-export.*` series.
