# Segment bus

## What the pi original did

`@juanibiapina/pi-powerbar` rendered a persistent powerline bar while producer sub-extensions pushed segment updates through `pi.events`. That global in-process event bus had no owner boundary, caller identity, dependency declaration, or grant check.

## The omp shape

This extension alone owns the `(examples.segment-bus, segment-bus)` `STATUS_RIGHT` contribution. `@omp.service("segments.publish", rev=1)` accepts typed atomic replacements from sibling extensions; consumers connect with `await omp.services.connect("segments.publish", rev=1)` only after declaring `"segments.publish@1"` in their manifest grants. This is the CONTROL-brokered boundary required by [00-overview.md §Extension services](../../docs/py/00-overview.md#extension-services): journal entries and agent messages are not an RPC substrate.

The service derives the publisher identity from the caller-scoped `omp.Context`, retains at most 8 segments and 1024 UTF-8 payload bytes per publisher, and refuses an over-quota replacement before changing state or emitting UI. Each accepted replacement sorts the aggregate by ascending publisher-declared priority, with publisher and key as stable tie-breakers, then issues exactly one `ui.set_status` effect. That effect is coalesced by the one owner key. Its `order=60` is only a default seed: the user's statusline layout for `(extension, key)` remains authoritative as specified by [07-ui.md §4.7](../../docs/py/07-ui.md#47-status-segments).

`[features.demo_publisher]` is a deliberately tiny consumer. Its activation hook connects through `omp.services`, publishes one typed segment, and therefore exercises the same grant and request path a separately packaged sibling uses. The old ambient emitter, subscription registry, per-producer widget mounts, and hand-built powerline transport are deleted.

## Gaps

- `docs/py/00-overview.md` §`@omp.service` / `omp.services` says service implementations obtain the caller with `omp.context()` (line 788), but frozen `omp.context` is the context-window module (`crates/py/python/omp/__init__.py:297`), not a callable. The usable frozen caller-scoped API is `omp.Context.current()` (`crates/py/python/omp/_context.py:89-96`), which this port uses.
