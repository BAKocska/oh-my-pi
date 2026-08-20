## What the pi original did

`@sreetej510/pi-prompt-manager` kept reusable prompts in its own storage and exposed a custom `/prompt` TUI for saving, editing, deleting, selecting, and pasting them. Selecting pasted text into the editor instead of submitting it, so the user retained control of the final message (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`).

## The omp shape

This focused port implements `save`, `list`, and `use`. Each save appends a declared `SavedPrompt` entry to `omp.state` at `StateScope.USER`; folding the typed, versioned log makes the latest save for a name win without a second source of truth. USER scope is the best frozen surface because reusable prompts should outlive one session and remain available across the authenticated principal's projects; `docs/py/09-journal.md` §`omp.state` defines exactly that cross-session, cross-project scope. The command declares `ui.Arg` rows with `<name> <text>` and `<name>` usage ghosts, and its dynamic argument completion source reads the same fold to complete saved names after `use` (`docs/py/07-ui.md` §4.15).

There is no custom TUI, JSON file, editor API call, or storage helper. A successful `use` returns `ui.Prompt(text, submit=False)` and nothing in this extension calls `ui.submit` or sets editor text. As `docs/py/07-ui.md` §4.15 says exactly: “selecting a saved prompt should populate the composer, not start a turn, leaving the user in control of the final message.”

## Gaps

- `omp.command` is documented at `docs/py/07-ui.md` §4.15, including the signature at line 1570, but the frozen root module `crates/py/python/omp/__init__.py:292-320` exports `renderer` and registry declarations without defining or re-exporting `command` (and `__all__` at lines 889-929 omits it). The implementation therefore uses the otherwise matching frozen `omp.ui.command` at `crates/py/python/omp/ui/__init__.py:653`.
- `ui.command(..., args=..., hint=..., arg_completions=...)` is documented to provide static usage ghosts and dynamic argument completion at `docs/py/07-ui.md:1626-1640`, but frozen `crates/py/python/omp/ui/__init__.py:653-656` discards every metadata argument and retains only `_command_handlers[name] = function`; no frozen arg-completion registry exists (`_completion_handlers` at lines 403-408 is only populated by the separate trigger decorator at lines 641-644). The port supplies the documented arguments, but the frozen host cannot discover either the ghosts or `_complete_saved_names`.
