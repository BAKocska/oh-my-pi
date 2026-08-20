## What the pi original did

`mitsupi` replaced the normal edit surface with multi-edit/patch support as part of a larger personal workbench (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`). Like the hash-anchored edit extensions in the same survey cohort, that replacement depended on tool-name ownership rather than a durable argument-dialect identity.

## The omp shape

This port declares the soft, environment-placed `edit@patch.1` family beside the existing `rep.1` and `hl.1` families. It deletes replacement-by-registration-order: the wire name remains `edit`, while the recorded `Rev` determines which historical schema produced a call and the live family determines which device is advertised (`docs/py/02-verdicts.md` §Revisions and the lift chain).

`patch.1` implements the two destination-owned cross-family edges explicitly. Its `lift()` accepts `RecordedCall` values only when the recorded identity and source arguments agree with either `Rev.parse("rep.1")` or `Rev.parse("hl.1")`. For a settled success, the shared verdict's resolved path, byte ranges, and replacement text are enough to build one `PatchArgs` value; `LiftedCall.of` canonically reserializes that value and the dialect-neutral verdict. Malformed records, unresolved faults, empty edits, and any unknown revision return `None` rather than guessing.

The three-family evidence does not produce a canonical hub family. Two pairwise destination steps suffice for the families represented here: `rep.1 -> patch.1` and `hl.1 -> patch.1`. Same-family adjacent revision steps still compose toward the live revision in registry order, while a foreign family jumps directly to the live revision. If any destination step returns `None`, the registry discards the partial walk and retains the original argument and verdict bytes verbatim. The cost therefore remains one direct lift arm per historical foreign family on the live destination, not an extra hub hop.

Execution converts the live typed edits to frozen `omp.env.Edit` values and commits them through one `omp.env.docs` lease with rebase and best-effort formatting. It does not parse a private patch language, directly write a file, or keep undo/revision state in Python; the Environment document authority owns those mechanisms (`docs/py/11-env.md` §Document leases).

## Gaps

None — every symbol this port needs is frozen.
