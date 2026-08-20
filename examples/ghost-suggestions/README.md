## What the pi original did

`@mrclrchtr/supi-prompt-suggestions` replaced the composer with a `CustomEditor` subclass and intercepted every call to `handleInput(data)`. It decoded Tab, Right, and Escape, accepted or dismissed dim ghost text itself, and synchronized suppression after every keystroke; its model-generated suggestions therefore coupled a cosmetic feature to the terminal input path.

## The omp shape

This port declares an empty, line-start `ui.Trigger` and supplies prompt candidates asynchronously from `omp.sessions.list()` plus kind-filtered `omp.sessions.journal(..., kinds=("omp.message",))` reads. A JSON `snippets` setting contributes static candidates. Each `ui.CompletionItem` carries its untyped suffix in `hint`; `refine_locally=True` leaves grown-query filtering, fuzzy re-ranking, cancellation, stale-result retention, rendering, acceptance, and dismissal to the harness's lock-free hint slot. There is no editor replacement, key handler, escape-sequence decoder, or acceptance callback. This follows `docs/py/07-ui.md` §4.12 and the deletion described in §5.4; historical reads follow `docs/py/09-journal.md` §“Historical session queries.”

## Gaps

- `omp.Context.settings` is documented as the resolved manifest-setting map in `docs/py/00-overview.md` §“Manifest” (the `settings` row and the validation paragraph), but frozen `crates/py/python/omp/_context.py` defines `Context` without a `settings` field. The exact access is marked at the call site; until the frozen class exports it, configured snippets cannot be read by a real callback.
- `omp.completion` is documented as a module-level Declare decorator in `docs/py/07-ui.md` §4 and §4.12, but frozen `crates/py/python/omp/__init__.py` does not export it. The implementation uses the available frozen `omp.ui.completion` from `crates/py/python/omp/ui/__init__.py`; trigger and fold behavior are otherwise present.
