# Copy Cut

## What the pi original did

`@shelken/copy-cut` bound `alt+shift+x` and cut the current Pi input-editor text to the system clipboard.

## The omp shape

This port declares `alt+shift+x` as a lazy shortcut and `/paste-cut` as a lazy command. The action reads the composer with `omp.ui.editor_text()`, refuses values over 64 KiB, writes the clipboard before making any destructive edit, appends one typed `LastCut` value to `omp.StateScope.SESSION`, and clears the composer with `omp.ui.set_editor_text("")`. `/paste-cut` reads only `omp.state.latest(...)` and replaces the composer with that value, so the interface is a bounded last-cut slot rather than a process-global kill ring. The raw terminal-input interception and direct terminal escape ownership are deleted: `docs/py/07-ui.md` §2.6 and §4.14 assign chord matching and consumption to Core, §4.16 owns composer effects, `docs/py/09-journal.md` §`omp.state` owns session state, and `docs/py/14-deploy.md` §3.1.5 owns the static shortcut/command declarations.

## Gaps

- Required symbol `omp.ui.set_clipboard(text: str) -> None` is absent. The frozen effect list in `crates/py/python/omp/ui/__init__.py:524-542` has no clipboard or OSC 52 verb, and `docs/py/07-ui.md` §4.16 (`:1688-1720`) documents no clipboard effect. Until that client-owned effect exists, the shortcut deliberately faults before clearing the composer rather than pretending a notification, subprocess, or raw escape copied the text.
- Frozen `omp.ui.shortcut` at `crates/py/python/omp/ui/__init__.py:704-707` only stores a callback: it neither validates the chord nor adds a shortcut declaration to `omp._declarations`. That diverges from `docs/py/07-ui.md` §4.14 (`:1521-1545`) and the generated declaration contract in `docs/py/14-deploy.md` §3.1.5 (`:652-691`), so this example must hand-author its manifest row.
