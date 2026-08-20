## What the pi original did

`pi-ask-user` built a custom two-pane terminal questionnaire with selection, multi-selection, notes, free-form input, fuzzy matching, paging, keybindings, timers, and abort handling. When that UI was unavailable, it maintained a separate five-step fallback through a custom overlay, `askDialog`, `select`, `input`, and `confirm`.

## The omp shape

The extension makes one `ui.ask_user` call with typed `ui.AskQuestion` and `ui.SelectItem` values. The native dialog owns layout, filtering, keybindings, countdown, and cancellation; its degradation to form/select/input RPCs or `DialogCancel.UNAVAILABLE` belongs entirely to the harness, as specified by `docs/py/07-ui.md` §4.10 and demonstrated in §5.2. On `UNAVAILABLE`, the device returns the original structured questions in its verdict so the model can relay them; it contains no dialog fallback chain. The frozen `rev: int` contract is authoritative, so the declaration uses `rev=1`.

## Gaps

- No dialog/form gap remains for this port: `ui.ask_user`, `ui.AskQuestion`, `ui.SelectItem`, `ui.DialogOptions`, `ui.DialogCancel`, and their used fields are exported by the frozen UI module.
