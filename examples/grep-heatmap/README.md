# grep heatmap

## What the pi original did

`pi-fovea` intercepted native grep results in `tool_result`, appended a token-budgeted graph heat-diffusion map to the returned model text, and injected unsolicited background sync messages with `sendMessage` (`catalog.md`, `pi-fovea`). That couples indexing, transcript presentation, and model context to mutation of one landed result.

## The omp shape

The landed `Ok` is immutable. A `tool_result` hook may not write `prompt`, `text`, `content`, or `parts` (`docs/py/05-hooks.md` lines 531–534), and a later denial records a separate postcondition rather than converting success into a fault (`docs/py/05-hooks.md` lines 1555–1561). This port therefore never rewrites a verdict and never reads or changes its `parts` or `details`.

The replacement separates the three jobs:

- `code_map` is a soft device the model calls explicitly through `dyn`. It returns typed `CodeMapResult` and `HeatedFile` values. The SQLite database under `await omp.state_dir()` is only a rebuildable index, matching `docs/py/09-journal.md` §`omp.state_dir` (lines 1230–1233); source remains authoritative.
- The `tool_result` OBSERVE hook refreshes the indexed document only after a native `edit` has settled as `Ok`. It also fold-syncs a small immutable heat annotation for a settled native grep call. It returns `None`, never injects a message, and is off the decision path as required by `docs/py/05-hooks.md` lines 704–708.
- `render_grep_augmentation` is the pure presentation suffix: it uses only `view.call_id`, the verdict arm, and the cached annotation, and emits TML without inspecting verdict payload parts. It is intentionally not registered as the one exact grep renderer because that would replace/race with the native renderer rather than augment it.

Thus the original result interception becomes a transcript-only suffix plus an explicit typed device; the graph cache becomes a replaceable `state_dir()` index; auto-sync becomes a settled-edit observer; and `sendMessage` background injection is deleted with no analogue. Model context changes only when the model deliberately calls `code_map`.

## Gaps

- **Missing renderer augmentation symbol:** the frozen `omp.renderer` at `crates/py/python/omp/ui/__init__.py:677` registers one exact-revision fold and `_device_renderers` stores exactly one callable per key (`:676`, `:685–688`); it has no augmentation/append mode. `docs/py/01-devices.md` lines 414–418 calls renderer decoration of a core tool the sanctioned presentation mechanism, while `docs/py/02-verdicts.md` lines 680–684 says the name must match the extension's registered device and describes only ownership of the whole fold. Until a distinct augmentation API is frozen and documented, registering this suffix for `grep@1` would incorrectly claim the complete native renderer and can collide with another exact renderer. The fully pure suffix is implemented and smoked, but deliberately left unregistered.
