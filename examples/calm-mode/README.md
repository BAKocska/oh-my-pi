# Calm mode

## What the pi original did

`pi-calm` kept the conversation and working indicator visible while hiding tool chatter and, optionally, assistant thinking. A slash command toggled the quieter view. Some variants reached into Pi's context/history representation to obtain that effect, coupling a display preference to the model-facing conversation.

## The omp shape

`/calm` appends one typed toggle at `omp.StateScope.SESSION`; it never rewrites a message, call outcome, prompt projection, or existing journal entry. The reasoning `@omp.message_renderer("reasoning")` returns empty TML only for the transcript surface. Exact `(name, rev)` verdict folds cover the built-in `read`, `bash`, `grep`, `glob`, `edit`, and `write` revision-1 identities and return a one-line status when the host supplies `RenderCtx.collapsed`; expanded rendering falls through unchanged. Thus collapsed and expanded views are projections of the identical retained verdict, and the journal and model context keep the complete call.

This deliberately deletes context rewriting, message replacement, result rewriting, tool re-registration, terminal-input interception, hand-rolled collapse state, and mutation of stored outcomes. `docs/py/07-ui.md` §4.13 lines 1443–1471 makes `RenderCtx.collapsed` the sanctioned read-only presentation fact. Its §6.4 non-goals, lines 2488–2490, explicitly drops Pi's `getToolsExpanded`/`setToolsExpanded`: an extension reads `RenderCtx.collapsed` and does not flip the user's fold preference. Device folds remain synchronous UI projections under `docs/py/02-verdicts.md` lines 656–711, and reasoning is the named message-renderer kind in `docs/py/07-ui.md` lines 1499–1524.

The only new journal fact is the invisible session toggle itself. It controls whether these projections participate; it is not conversational content and is never projected to the model.

## Gaps

- **A synchronous pure renderer cannot read a toggle stored at SESSION scope.** `docs/py/07-ui.md` lines 1467–1471 requires determinism solely in `(state, RenderCtx)` and explicitly forbids mutable-global reads. Frozen `omp.View` at `crates/py/python/omp/_verdicts.py:298-307`, `omp.ui.RenderCtx` at `crates/py/python/omp/ui/__init__.py:332-335`, and `omp.ui.MessageView` at `:338-345` contain no session presentation state. `omp.state.latest` is async (`crates/py/python/omp/__init__.py:236-238`) and therefore cannot be called by the synchronous folds. This worked example restores and updates a small in-process snapshot at activation/command time, matching the existing frozen callback constraints, but that snapshot makes replay depend on external mutable state and does not satisfy the documented strict purity rule. The frozen layer needs a host-snapshotted renderer preference/state field (or an equivalent immutable dispatch input) for a fully pure toggleable port.
- **The renderer documentation still contradicts the frozen core-tool spelling.** `docs/py/02-verdicts.md` lines 658–680 documents no `decorates` parameter and says `name` MUST match a device registered by the extension. The frozen decorator accepts any exact key at `crates/py/python/omp/ui/__init__.py:703-724`, while `docs/py/07-ui.md` lines 1481–1486 adds `decorates=True` as the sanctioned way to augment a core renderer. Augmentation cannot replace/hide the base row, so calm mode's one-line replacement currently relies on the frozen exact-key base registration rather than the newer documented core-decoration rule.
