## What the pi original did

`pi-powerline-footer` replaced Pi's footer renderer and assembled ANSI-styled powerline strings, while replaying many streaming, tool, model, and session events to reconstruct current status. It also owned editor modes, hotkeys, queue widgets, git/context/token indicators, and working messages. Much of that event plumbing existed only to keep terminal chrome current.

## The omp shape

This port contributes model, context-pressure, and telemetry-fed token segments plus one footer mount using TML; it has no width arithmetic, ANSI styling, theme threading, editor replacement, or raw terminal-input handler because the harness owns layout and rendering (`docs/py/07-ui.md` §5.1). It paints only on activation and settled turns, hashes the render inputs, and relies on the keyed `set_status`/`mount` state effects, so identical facts cause no update and no work runs per keystroke. The `order=` values only seed the default: under the attribution ruling (`docs/py/07-ui.md` §§2.9 and 4.7), the user's layout preference wins.

## Gaps

- `omp.ui.mount` is present in the frozen layer with the documented `mount(Slot, Tml, SlotOptions | None, *, key=...)` signature. There is no current signature gap; it is listed here because the slot-mount boundary was an explicit concern for this worked port.
- `omp.ui.set_status` is also present with the documented keyword-only `order` and `side` parameters. There is no current signature gap; its keyed coalescing semantics are the status contribution boundary used here.
