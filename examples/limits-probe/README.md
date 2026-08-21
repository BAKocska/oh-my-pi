# What this probes

This is a mechanical conformance probe, not a port; there is no pi origin. It inventories every ceiling exposed by the frozen Python layer, compares it with the normative docs, and drives both sides of every boundary reachable without a live host. `limits_probe.smoke()` also verifies that a refusal is typed, identifies its limit, and leaves declaration state unchanged where the local API owns mutation.

## Boundary matrix

`—` means the boundary is enforced by Core or another live-host component and cannot be reached by the no-I/O stub. “Before mutation” is only asserted where mutation is observable; descriptive constants do not themselves accept input.

| Constant / contract | Frozen value | Documented value | At limit | Over refuses | Typed | Names limit | Before mutation |
|---|---:|---:|---|---|---|---|---|
| `omp.MAX_DECLARATIONS` | 256 | 256 | yes | yes | yes, `DeclarationLimit` | yes | yes |
| `omp.MAX_WORKERS` | 8 | 8 | host-only | — | — | — | — |
| `omp.workers.RESULT_SPILL_BYTES` | 262,144 | 262,144 | host-only | — | — | — | — |
| `omp.devices.HARD_SLOT_BUDGET` | 8 | 8 | host-only | — | — | — | — |
| `omp.devices.PER_DEVICE_CAP` | 10,000 | 10,000 | host-only | — | — | — | — |
| `omp.devices.EXTERNAL_SUMMARY_CAP` | 200 | 200 | host-only | — | — | — | — |
| `omp.BASH_IR_MAX_SOURCE` | 262,144 | 262,144 | analyzer snapshot only | — | typed result promised | yes | host-only |
| `omp.BASH_IR_MAX_NODES` | 50,000 | 50,000 | analyzer snapshot only | — | typed result promised | yes | host-only |
| `omp.BASH_IR_MAX_DEPTH` | 128 | 128 | analyzer snapshot only | — | typed result promised | yes | host-only |
| `omp.POLICY_DEADLINE` | 30s | 30s | host-only | — | typed denial promised | yes | before effects |
| `omp.APPROVAL_DEADLINE` | 5m | 5m | host-only | — | typed ticket state promised | yes | host-only |
| `omp.VIOLATION_COALESCE` | 1s | 1s | descriptive window | — | — | — | — |
| `omp.limits.REENTRANCY_DEPTH` | 4 | 4 | host-only | — | — | — | — |
| `omp.limits.INTERACTIVE_CAP` | 15m | 15m | host-only | — | — | — | — |
| `omp.limits.SETTLE_CONTINUATION_CAP` | 8 | 8 | host-only | — | — | — | — |
| `omp.limits.SHUTDOWN_BUDGET` | 2s | 2s | host-only | — | — | — | — |
| `omp.limits.OBSERVE_CAP` | 64 | 64 | host-only | truncates by contract | no refusal | warning names cap | no mutation contract |
| `omp.limits.MODIFY_ROUNDS` | 1 | 1 | descriptive | — | — | — | — |
| `omp.journal.MAX_INLINE_BYTES` | 65,536 | 65,536 | host-only | — | — | — | — |
| `omp.journal.MAX_ENTRY_BYTES` | 16,777,216 | 16,777,216 | host-only | — | — | — | — |
| `omp.journal.MAX_LABEL_BYTES` | 256 | 256 | host-only | — | — | — | — |
| `omp.journal.MAX_ATOMIC_ENTRIES` | 1,024 | 1,024 | host-only | — | — | — | atomicity promised |
| `omp.agents.DEFAULT_MAX_DEPTH` | 2 | 2 | host-only | — | — | — | — |
| `omp.agents.DEFAULT_MAX_CONCURRENCY` | 32 | 32 | host-only | — | — | — | — |
| `omp.agents.DEFAULT_CONTINUATION_CAP` | 8 | 8 | host-only | — | — | — | — |
| `omp.agents.STEER_GRACE` | 500ms | 500ms | host-only | — | — | — | — |
| `omp.agents.MIN_SCHEDULE_INTERVAL` | 30s | 30s | host-only | — | typed declaration refusal promised | yes | before schedule creation |
| `omp.agents.MAILBOX_CAPACITY` | 100 | 100 | host-only | FIFO eviction | no refusal | — | mutates by contract |
| `omp.agents.MAX_BACKFILL` | 32 | 32 | host-only | coalesces remainder | no refusal | — | mutates by contract |
| `omp.agents.EMPTY_OUTPUT_RETRY_CAP` | 3 | 3 | host-only | — | — | — | — |
| `omp.telemetry.QUEUE_DEFAULT` | 4,096 | 4,096 | descriptive | — | — | — | — |
| `omp.telemetry.QUEUE_MAX` | 65,536 | 65,536 | yes | yes | yes, `SubscriptionError` | yes, numeric range | yes |
| `omp.telemetry.BATCH_MAX` | 1,024 | 1,024 | yes | yes | yes, `SubscriptionError` | yes | yes |
| `omp.telemetry.QUERY_LIMIT_MAX` | 10,000 | 10,000 | host-only | — | — | — | — |
| `omp.telemetry.METRIC_PREFIX` | `omp.ext.` | `omp.ext.` | descriptive | — | — | — | — |
| `omp.telemetry.MAX_INSTRUMENTS` | 256 | 256 | yes | yes, instrument 257 | yes, `SubscriptionError` | yes | yes |
| `omp.telemetry.MAX_CARDINALITY` | 1,024 | 1,024 | yes | folds into `overflow="true"` | no refusal | one local and one host cardinality warning | no 1,025th series |
| `omp.telemetry.DEFAULT_MAX_BYTES` | 51,200 | 51,200 | descriptive | — | — | — | — |
| `omp.telemetry.DEFAULT_MAX_LINES` | 3,000 | 3,000 | descriptive | — | — | — | — |
| `omp.telemetry.DEFAULT_MAX_COLUMN` | 512 | 512 | descriptive | — | — | — | — |
| `omp.telemetry.SPILL_BYTES` | 51,200 | 51,200 | descriptive | — | — | — | — |
| `omp.telemetry.SPILL_LINES` | 3,000 | 3,000 | descriptive | — | — | — | — |
| `omp.telemetry.SPILL_COLUMN` | 512 | 512 | descriptive | — | — | — | — |
| `omp.SPILL_INLINE_LIMIT` | 16,384 | 16,384 | descriptive | spill policy object | typed by host | host-only | host-only |
| `omp.ui.limits.TML_MAX_BYTES` | 262,144 | 262,144 | yes | yes | yes, `TmlError` | yes | yes, before parse/allocation |
| `omp.ui.limits.TML_MAX_DEPTH` | 64 | 64 | yes | yes | yes, `TmlError` | yes | yes, while scanning depth |
| `omp.ui.limits.SLOT_MAX_PER_EXTENSION` | 16 | 16 | host-only | — | — | — | — |
| `omp.ui.limits.NOTIFY_PER_TURN` | 10 | 10 | host-only | — | — | — | — |
| `omp.ui.limits.COMPLETION_DEADLINE` | 250ms | 250ms | host-only | — | — | — | — |
| `omp.ui.limits.RENDER_DEADLINE` | 50ms | 50ms | host-only | — | — | — | — |
| `omp.ui.limits.OVERLAY_MAX_CONCURRENT` | 2 | 2 | host-only | — | — | — | — |
| `omp.ui.limits.WATCH_DEBOUNCE` | 60ms | 60ms | host-only | — | — | — | — |
| `omp.MAX_FRAME_BYTES` | 67,108,864 | 67,108,864 | transport-only | yes | `FrameTooLarge` | numeric limit | before send/decode |
| `omp.MAX_PENDING_EFFECTS` | 1,024 | 1,024 | host-only | shared request/effect bound | host-owned | yes | effect mailbox drops oldest |
| `omp.MAX_HOST_CHILDREN` | 32 | 32 | host-only | — | typed install failure promised | host-only | before install |
| `omp.DOCS_TOTAL_BUDGET` | 48,000 | 48,000 | host-only | — | — | — | — |

## Closure records

1. **Closed — TML byte and depth ceilings.** The former constructor gap was fixed in `crates/py/python/omp/ui/__init__.py:115-153`: UTF-8 byte length is checked before parsing, and nesting is checked while scanning opening tags. Re-observation accepts exactly 262,144 bytes and 64 levels, then raises a limit-naming `TmlError` for one byte or level over.
2. **Closed — rendered-result telemetry constants.** The missing `DEFAULT_MAX_BYTES`, `DEFAULT_MAX_LINES`, `DEFAULT_MAX_COLUMN`, `SPILL_BYTES`, `SPILL_LINES`, and `SPILL_COLUMN` exports now live at `crates/py/python/omp/telemetry.py:31-42`. Re-observation reads 51,200 bytes, 3,000 lines, and 512 UTF-16 columns from both constant families.
3. **Closed — metric quota values, enforcement, and host visibility.** The binding 2026-08-20 ruling is recorded at `docs/py/10-telemetry.md:1112-1125`. Its constants are frozen in `crates/py/python/omp/telemetry.py`; instrument creation refuses instrument 257 with `SubscriptionError` before registry mutation, while `_bounded_attrs` retains exactly 1,024 series and folds later series into `overflow="true"`. On the first overflow per instrument it now publishes one fail-open `host_warning` effect carrying `code="cardinality"` and the fully qualified instrument name, while preserving the one local `RuntimeWarning`; later overflowing series emit neither warning again. The smoke re-observes both boundaries, the unchanged fold, and the once-only warning behavior with and without an effect sink.
4. **Closed — frame ceiling.** `omp.MAX_FRAME_BYTES` now exports 67,108,864 from `crates/py/python/omp/limits.py:24-25`; `crates/py/python/omp/_host.py:102-109` applies the same value before decode and before send. The frozen value now equals the documented transport ceiling.
5. **Closed — aggregate device-doc budget.** `omp.DOCS_TOTAL_BUDGET` and `omp.limits.DOCS_TOTAL_BUDGET` now export 48,000 from `crates/py/python/omp/limits.py:18-19`, matching the corrected public names in `docs/py/01-devices.md:1070-1072`.
6. **Closed — pending-effects ceiling surface.** `omp.MAX_PENDING_EFFECTS` now exports 1,024 from `crates/py/python/omp/limits.py:30-31`, and the frozen transport imports that single value at `crates/py/python/omp/_host.py:23-25`. Effect-mailbox drop-oldest behavior remains Core-owned and is therefore not reachable in this no-I/O probe.

## Still-open observations

- Closed during remediation: `omp.telemetry.QUEUE_MAX = 65_536` and `omp.telemetry.QUERY_LIMIT_MAX = 10_000` are now exported and wired into queue and query validation (`crates/py/python/omp/telemetry.py`), replacing the former anonymous literals.

## Stub smoke

Run from the repository root after installing the gate's native stubs:

```sh
python3 -c 'import importlib.util, pathlib, sys; p=pathlib.Path("scripts/check-python-examples.py"); s=importlib.util.spec_from_file_location("gate", p); m=importlib.util.module_from_spec(s); sys.modules["gate"]=m; s.loader.exec_module(m); m._install_native_stubs(); sys.path[:0]=["crates/py/python", "examples/limits-probe"]; import limits_probe; limits_probe.smoke()'
```

The smoke asserts the ruled TML and telemetry quota contracts directly: over-limit TML is rejected, instrument 257 is refused without mutation, and series 1,025 is folded into `overflow="true"`. A dedicated remediation stub also verified that the first overflow publishes one `host_warning` effect with `code="cardinality"` and the prefixed instrument name, later overflow emits no duplicate, and detaching the effect sink still leaves one local `RuntimeWarning` without breaking metric recording.
