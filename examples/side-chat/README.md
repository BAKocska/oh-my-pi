## What the pi original did

`pi-btw` opened `/btw` as a custom interactive TUI modal so the user could hold parallel side conversations beside the primary session. It stored those conversations as custom entries in the main session rather than as independent agent journals (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`, `pi-btw`).

## The omp shape

`/btw [question ...]` opens one native `omp.ui.overlay` and keeps the main composer and transcript untouched. Selecting “New side thread” and sending a question spawns `omp.agents.SubagentSpec(background=True)`; selecting an existing thread sends the follow-up through `omp.agents.send`. Every side conversation is therefore a real child journal with a Core-minted `history://` reference, not a custom-entry transcript. The modal reads the selected handle's typed `HistoryUrl`, shows its bounded tail, and can refresh without copying the child exchange into the main journal (`docs/py/07-ui.md` §4.9, `docs/py/12-agents.md` §§Spawning, The handle, and Inter-session messaging, and `docs/py/09-journal.md` §URL resolution).

Only child `session_id` strings are appended in a declared `SideThreadIds` snapshot at `StateScope.SESSION`. Handles are disposable: `/btw` re-resolves each saved id with `omp.agents.get`, including after host restart, while the child journal remains the sole transcript truth (`docs/py/09-journal.md` §`omp.state`; `docs/py/12-agents.md` §Listing, revival, and limits and lines 2489–2493). The command never waits for a child and installs no continuation hook. Because every spawn is `background=True`, Core owns the job and posts settlement as a `TurnBoundary` interrupt; it cannot preempt or extend the main turn (`docs/py/12-agents.md` lines 322–330 and 464–469). The original custom-entry thread store, renderer factory, subprocess lifetime, and settlement polling are deleted.

## Gaps

- `omp.urls.read(url, selector=None)` is documented at `docs/py/09-journal.md:1066-1077`, but frozen `crates/py/python/omp/urls.py:334-348` exports no `read` symbol. The typed `HistoryUrl.read()` used here is present through `crates/py/src/bindings.rs:651-656`, so transcript rendering works, but the documented namespace function remains absent.
- `OverlayHandle.events()` is documented to yield watched `HIGHLIGHTED`, `CHANGED`, `FILTERED`, and `PRESSED` interactions before one terminal event (`docs/py/07-ui.md:1126-1129,1145-1149,1166-1176`), while frozen `crates/py/python/omp/ui/__init__.py:479-481` only awaits `wait()` and synthesizes a terminal `SUBMIT` or `CANCEL`. This port uses a transactional submit/reopen loop rather than masking that divergence with a second event system.
