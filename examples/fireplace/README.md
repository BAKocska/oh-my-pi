## What the pi original did

`@jpodivin/pi-fireplace` added a cozy animated fireplace to Pi, including continuously moving flames and smoke the user could toggle (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`). In that component model, animation naturally meant an extension-owned frame timer that repeatedly repainted terminal content.

## The omp shape

`/fireplace` mounts or unmounts one keyed `Slot.FOOTER` tree; `/fireplace smoke` changes the root node's `value` prop, and the smoke row's `when='fireplace-root=on'` expression removes or restores it without rebuilding the tree. The initial value comes from `[settings].smoke`. The fire and smoke glyphs are always emitted through `ui.icon`, so the renderer owns charset selection; the ASCII tier reduces the flames to the humble `(fire)` rather than embedding a Unicode codepoint (`docs/py/07-ui.md` §§4.4–4.6).

The flames declare `spin`, `anim`, and `ease`, while the labels declare `shimmer`; the shared Rust animation clock paints them. There is no Python timer, frame loop, sleep, repaint task, terminal escape, or width calculation. The original frame-timer mechanism is deleted rather than translated. `<scene>` and `<shader>` are also intentionally absent: they are Rust-only, not frozen extension TML (`docs/py/07-ui.md` §4.2, especially lines 613–615; property semantics are in §4.3, lines 677–702). The frozen constructors and mount/patch surface used here are `ui.icon`, `ui.tml`, `ui.presentation`, `ui.mount`, `ui.handle`, and `SlotHandle.patch` (`crates/py/python/omp/ui/__init__.py:141-164, 286-301, 486-516`).

## Gaps

None.
