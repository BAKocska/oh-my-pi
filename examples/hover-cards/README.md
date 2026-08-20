## What the pi original did

`pi-cc-extensions` enriched collapsed tool cards and took ownership of terminal mouse reporting, including a direct `tui.terminal.write("\\x1b[?1000h")` to enable VT200 click reporting and a guarded disable write on teardown. Its presentation layer used those raw pointer events to recolor, raise, and open tool details.

## The omp shape

This port makes `hover_card` a small extension-owned worked result and renders it with an id-bearing, focusable `<box>` carrying `hover='accent..info'`, `lift`, `anim=180ms`, and `ease=in-out`. Focus changes deterministically alter the semantic border token, while the TUI owns pointer negotiation and animation (`docs/py/07-ui.md` §2.8, lines 228–248, and §4.3, lines 677–702). `detail_overlay()` and `open_detail()` use the frozen `ui.overlay`/`OverlayHandle.wait` surface for the modal detail view; there are no terminal writes, input handlers, geometry calculations, or control bytes.

The detail tree intentionally wraps its ordinary child in `<approval>`. Per `docs/py/07-ui.md` §2.9 and §4.2 (lines 298–300 and 621–625), extension-origin parsing removes reserved chrome names from the element catalog, so this becomes a `CustomElement` that paints only its child rather than Core approval chrome.

## Gaps

- An id-bearing focusable `<box>` is documented as emitting activation on click or Enter (`docs/py/07-ui.md` lines 243–246 and 690–692), but the frozen Python layer exposes no transcript activation payload, decorator, registration table, or dispatch callback. `crates/py/python/omp/ui/__init__.py:220-222` defines `EventKind.PRESSED` only for retained-overlay watches; `:585-590` and `:647-655` expose message-renderer, shortcut, and command registrations but no element-activation registration. Consequently `open_detail()` is complete and smokeable, but the card's emitted activation cannot be connected to it without inventing an API. This is a frozen-vs-docs defect, not replaced with a raw-input or shortcut shim.
