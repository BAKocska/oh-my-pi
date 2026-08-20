# Welcome Chrome

## What the pi original did

`@zeerke/ascet-copilot-ui` replaced Pi's startup chrome with an animated branded header, recent-session choices, rotating tips, a working spinner, and an animated terminal title.

## The omp shape

The host owns the welcome scene and its alternate-buffer lifetime. This extension mounts only a retained `Slot.HEADER` document; the TUI doctrine sanctions the alt buffer for overlays and the welcome scene while keeping chat and transcripts inline and mouse-selectable (`AGENTS.md:371-375`, `crates/tui/src/runtime.rs:318-321,591-594`). It never enters or leaves a terminal mode itself.

`session_start` mounts the header and uses `omp.sessions.list(SessionFilter(...))` to build typed `ui.SelectItem` rows. Because slots are deliberately non-interactive, recent sessions are offered through the native `ui.select` picker; accepting a row calls the frozen `omp.sessions.resume(session_id)` CONTROL verb (`docs/py/07-ui.md` §4.2, lines 496-499; `docs/py/09-journal.md` §Session mutation, lines 686-713). The header stays user-layout-arbitrated: `SlotOptions.order` only seeds a default and user layout wins (`docs/py/07-ui.md` §2.9, lines 307-310).

Tips come from `[settings].tips` and advance only at lifecycle state changes. `anim`, `ease`, `spin`, `shimmer`, `reveal`, named icons, and `<spinner>` are declarative retained-tree properties driven by the TUI's shared animation clock (`docs/py/07-ui.md` §4.3, lines 677-702). The named `sparkles`, `lightbulb`, `history`, and `play` icons carry ASCII fallbacks in `crates/tui/icons.tsv`; Python never chooses a glyph or runs an animation timer.

`agent_start` sets the sanitized terminal title with `ui.set_title` and changes the retained header to its activity spinner. `agent_end` restores omp's generated title with `ui.set_title(None)` and returns the header to idle; `session_shutdown` unmounts the slot and restores the title again (`docs/py/07-ui.md` §4.8, lines 1029-1038). The original terminal-title animation loop is deleted: there is no title frame loop, sleep, task, ANSI/OSC write, or direct terminal handle. The startup-chrome takeover is deleted too: attribution, placement, terminal modes, geometry, charset degradation, and final ordering remain host-owned.

## Gaps

None.
