## What this probes

There is no pi origin. This is a conformance probe for omp's byte-stability contracts. Every byte-stability row renders twice from the same value and compares bytes, then perturbs one field and requires different bytes. The standalone smoke also imports cheap, pure projections from `canary`, `chat-bridge`, and `edit-dialect`; it does not import peers from the hosted device path because declarations are sealed before dispatch.

The canonical-codec row additionally requires compact, sorted-key UTF-8 bytes (including a non-ASCII value), rejects a non-finite number, requires exact typed round-trip across a dataclass, enum, bytes, primitives, containers, and `Any`/`object` canonical-JSON fields, and requires `VerdictShapeError` for a type mismatch and trailing data. The entry-kind row requires decode identity as well as repeat and perturb byte checks.

## Re-observed matrix (2026-08-20)

| Contract | Stable under identical | Changed under perturbation | Lint fires |
|---|---:|---:|---:|
| Stable prompt contribution | yes | yes (`cwd`) | n/a |
| Deliberately volatile prompt contribution | rejected, as required | n/a | yes (`omp.prompts.VolatilePrompt`) |
| Verdict prompt projection | yes | yes (report row) | n/a |
| `Device.lift` | yes | yes (recorded args) | n/a |
| `LiftedCall.of` canonical bytes | yes | yes (dataclass field) | n/a |
| Public `omp.dumps` / `omp.loads` canonical codec | yes; exact compact sorted-key UTF-8 and typed round-trip | yes (`label`) | n/a |
| Catalog/docs declaration rendering | yes | yes (device removed) | n/a |
| Entry-kind encode/decode round-trip | yes; decoded value equals input | yes (`label`) | n/a |
| `canary.canary_prompt` corpus sweep | yes | yes (`session_id`) | n/a |
| `chat_bridge._reply_prompt` corpus sweep | yes | yes (reply text) | n/a |
| `edit_dialect._DialectProjection.lift` corpus sweep | yes | yes (recorded patch) | n/a |
| `edit_dialect._DialectProjection.prompt` corpus sweep | yes | yes (result revision) | n/a |

`stable_probe_prompt` and `deliberately_volatile_prompt` are real `@omp.prompt_slot` declarations. `_checked_prompt` applies the documented identical-input, two-render comparison. The volatile slot's exact source of churn is `_VOLATILE_COUNTER`, a mutable module global, and the probe requires the mismatch to raise rather than ship. `determinism_probe.prompt` and `determinism_probe.lift` are the projection and lift methods of the declared device. The catalog row consumes the real frozen Python declaration snapshot and sorts by `(name, family, rev)` before rendering the catalog/docs fields.

## Numbered closure records

1. **Canonical codec surface — closed.** The defect was that the documented `omp.dumps` and `omp.loads` symbols and typed decoder were absent, blocking canonical argument serialization and entry-kind round-trip. The implementation now provides deterministic canonical encoding at `crates/py/python/omp/_verdicts.py:721-735` and strict typed decoding at `crates/py/python/omp/_verdicts.py:906-949`; the public imports are at `crates/py/python/omp/__init__.py:196-198` and exports at `crates/py/python/omp/__init__.py:1703-1706`. Re-observation produced the exact expected compact, sorted-key UTF-8 bytes; rejected non-finite input; raised `VerdictShapeError` for both a typed mismatch and trailing data; preserved canonical JSON through `Any` and `object` fields; round-tripped the reachable dataclass, enum, bytes, container, and primitive values; and preserved `ProbeEntry` identity after encode/decode. Repeated input was byte-identical, while the selected field perturbations changed bytes.
2. **Top-level volatile-slot error — closed.** The defect was that the documented `omp.VolatilePrompt` path was absent even though `omp.prompts.VolatilePrompt` existed. The top-level import is now at `crates/py/python/omp/__init__.py:745` and the public export at `crates/py/python/omp/__init__.py:1700`. Re-observation confirms `omp.VolatilePrompt` is a type, and the deliberately changing slot is rejected with `omp.prompts.VolatilePrompt` on the identical-input two-render check.

All 12 re-observed checks are available and conforming. No findings remain open.
