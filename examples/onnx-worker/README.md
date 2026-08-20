## What the pi original did

`pi-onnx` loaded Hugging Face `onnx-community` text-generation and embedding models into pi's process and registered a chat provider plus inference tools (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:332`). This worked locally, but the native runtime shared the agent's crash and scheduling fate.

## The omp shape

This worked port keeps one deterministic text-generation/embedding session as a dependency-free stand-in for the real ONNX session. `local_infer` runs at `place="worker:onnx"`; its `WorkerSpec` selects `Site.LOCAL`, warms one session per generation, bounds native-cache lifetime and resources with `WorkerResources`, and restarts after failure (`docs/py/04-placement.md` §4, “Native-crash isolation”). The bespoke subprocess client, unavailable stub, refcounting, and crash-tail plumbing are deleted because the named-worker supervisor owns them.

The process boundary provides two separate isolation wins. First, a native ONNX crash kills and restarts only this worker generation, not the extension host or agent. Second, if an installed native wheel re-enables the GIL, only this extension's process becomes serial; the child reports the post-import flip loudly to the journal and continues rather than refusing the wheel, as resolved in `docs/py/00-overview.md` open question 6. Large JSON results return `omp.Spill`, so bytes over `omp.workers.RESULT_SPILL_BYTES` enter the central blob store instead of crossing through the host or becoming a temporary path (`docs/py/04-placement.md`, “Large payloads”).

## Gaps

- `omp.workers.RESULT_SPILL_BYTES` is `1_048_576` in frozen `crates/py/python/omp/placement.py:166`, while `docs/py/04-placement.md` §“omp.workers” specifies `262_144` at lines 882–888. This port follows the frozen constant, so the central spill path is correct but begins at the frozen threshold.
