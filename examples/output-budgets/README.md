# Output budgets

## What the pi original did

The three origins all sat downstream of already-formatted strings. `pi-rtk-optimizer` rewrote eligible shell calls and compacted or sanitized streamed and final output; `pi-slim-tools` replaced the output of seven tools with one-line summaries and bound `Ctrl+O` to reconstruct an expanded view; `pi-lean-ctx` disabled core tools and routed reads and shell output through an external compressor. The survey records those mechanisms at `catalog.md:108`, `catalog.md:111`, and `catalog.md:363`.

`docs/py/02-verdicts.md` §4 identifies the category error:

> `pi-slim-tools`' `Ctrl+O` expansion is the tell: the information the user wants on expand was thrown away, so the extension re-derives it.

## The omp shape

The category is declaration, not a rewrite extension. `OutputBudgetDevice` returns an `OutputPayload` containing every detail line and the typed `error`; its `prompt()` uses `omp.Budget`, so the harness-supplied `PromptCaps.maximum_text_bytes` limits only what the model sees. The smoke uses a deliberately tight 80-byte cap. Refusing a fragment changes no durable data (`docs/py/02-verdicts.md` §`omp.Budget`).

The device declares `__spill__ = omp.SpillBudget(inline_limit=1024, always=True)`. This is the Python declaration mirrored by Core's `SpillPolicy`, not a second spill implementation. The resolved ruling in `docs/py/02-verdicts.md` §5 says that `always` applies only above the roughly 1 KiB floor: a 200-byte verdict never becomes a blob write plus a journal reference. Larger verdicts use the central artifact gate and remain whole (`docs/py/02-verdicts.md` §Artifactization).

`OutputPayload.useless()` marks only superseded outcomes. During compaction, the `DropParts` amendment clears the tool-result's model-visible `content` while retaining `details`, `error`, `useless`, and `omp/tool-rev` (`docs/py/02-verdicts.md` §“An amendment that drops a projection and keeps the verdict”). `render_output()` never parses projected text: collapsed mode renders the typed label and count; expanded mode reads the same retained `OutputPayload.details` and `.error`. The information is never thrown away, so expansion costs only a repaint.

The complete port of each origin is therefore three declarations:

```python
# pi-rtk-optimizer
prompt = omp.Budget(caps)                         # bounded model projection
__spill__ = omp.SpillBudget(always=True)          # central spill policy
renderer = omp.renderer("output_budget")          # typed full-detail fold
```

```python
# pi-slim-tools
prompt = omp.Budget(caps)                         # terse model parts
useless = payload.useless()                       # superseded parts may drop
renderer = omp.renderer("output_budget")          # Ctrl+O reads retained details
```

```python
# pi-lean-ctx
prompt = omp.Budget(caps)                         # model-specific byte ceiling
__spill__ = omp.SpillBudget(always=True)          # no temp files or side cache
renderer = omp.renderer("output_budget")          # full truth stays available
```

Those lines are illustrative spellings of the declarations implemented in `output_budgets.py`; no output parser, replacement core tool, external compressor, cache invalidator, or raw-key handler survives.

## Gaps

None. `DropParts` is deliberately a Core-owned compaction amendment rather than a Python callable; the extension declares eligibility through `Payload.useless()` and continues to render the retained typed verdict.
