## What the pi original did

`@joyanhui/pi-ext-git-changes` added an interactive working-tree changes viewer with an overlay, a shortcut, and footer status. The original extension kept that footer current by running `git status` every second.

## The omp shape

This port takes one NUL-delimited `git status --porcelain=v1 -z` snapshot through `omp.env.sh.run`, so spaces remain part of a path and rename/copy records consume their required second NUL-delimited path. The frozen Environment owns execution and typed completion receipts (`docs/py/11-env.md` §6, `omp.env.sh.run` at lines 967-984).

A keyed `ui.set_status` contribution shows the dirty-file count and a BLAKE2s digest suppresses byte-identical repaint requests (`docs/py/07-ui.md` §4.7). Activation seeds it once. The `alt+shift+g` shortcut and `/changes` command open the same retained overlay; watched selection events render the selected path's bounded unified body through `<diff context=3>`, while the Refresh button explicitly resnapshots both the list and footer (`docs/py/07-ui.md` §§4.9, 4.14, and 4.15). The diff helper preserves line boundaries while applying the frozen TML wire escapes before `Tml.raw`; an ordinary string placeholder would intentionally strip C0 newlines under §4.1. Shell quoting is applied only after the selected opaque row id maps back to a parsed Git path. Oversized command output remains in its Environment `BlobRef` and only a bounded prefix enters TML.

Deleted mechanisms: the per-second `git status` timer, raw terminal input interception, ANSI diff coloring, terminal geometry arithmetic, and any background refresh loop. The only loop consumes host-fed watched overlay interactions; it performs no polling.

## Gaps

- TML `<diff>` is frozen and built at `crates/tui/src/markup.rs:1055` and `crates/tui/src/markup.rs:1226-1232`, but `docs/py/07-ui.md` §4.2 lines 608-615 says the tag is unavailable, while §6 lines 2305-2309 still describes adding it as proposed work. Those documentation sections contradict the frozen markup surface.
- TML `<diff context=N>` is named in `docs/py/07-ui.md` §6 lines 2305-2308, but the frozen property vocabulary has no `Context("context")` symbol at `crates/tui/src/props.rs:302-443`; `crates/tui/src/markup.rs:1226-1232` builds `DiffView` without reading such a property. The tag renders the unified body, and this port also asks Git for `--unified=3`, but the documented `context` markup property is not wired in the frozen renderer.
