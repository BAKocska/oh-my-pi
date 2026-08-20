# fzf actions

## What the pi original did

`pi-fzf` registered fuzzy-finder commands whose candidates came from arbitrary shell commands. It spawned `fzf`, handed the child process the terminal, then inserted, sent, or executed a template containing the selected line.

## The omp shape

`/pick <name>` reads a named picker from `[settings].pickers`, runs its `candidates_cmd` through the bounded Environment shell, and passes tab-separated rows to the native `omp.ui.select` dialog as aligned `SelectItem.cells`. The TUI owns filtering, focus, cancellation, and layout (`docs/py/07-ui.md:1189-1264`); the extension never spawns `fzf`, reads keys, emits ANSI, or takes over the TTY.

`insert-into-composer` uses the composer paste effect and never submits (`docs/py/07-ui.md:1698-1725`). `run-command` requires a configured argv array containing `{}`. Substitution happens in Python, but every completed argument crosses the shell boundary in a dedicated environment value and expands as exactly one quoted argv element; candidate bytes never become shell source. Candidate discovery and action execution use the Environment's guarded, bounded command path (`docs/py/11-env.md:930-984`). `open-overlay-preview` places the selected text in an escaped retained overlay and waits for native cancellation (`docs/py/07-ui.md:1074-1153`).

The deleted mechanisms are the `fzf` subprocess, its private fuzzy matcher and repaint loop, direct terminal input, and TTY takeover.

## Gaps

None.
