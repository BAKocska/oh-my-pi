# Session browser

## What the pi original did

`@vanillagreen/pi-session-manager` provided a polished terminal overlay for browsing recent sessions, searching titles, resuming a selected session, changing its title, and deleting it with a safety prompt.

## The omp shape

`/sessions` reads at most 200 immutable rows from `omp.sessions.list()`, keeps the frozen newest-activity ordering, and renders the indexed turn, token, and nano-USD cost fields as native TML columns. The filterable `<select>` searches its already-presented titles in the client; the extension never reads JSONL, walks a session directory, or builds a second index (`docs/py/09-journal.md` §“The sanctioned historical read API”; `docs/py/07-ui.md` §§4.4, 4.9, 4.15).

The picker stages its selected action in the composer with `submit=False`, so the user sees and submits an explicit command. Delete is deliberately not a silent overlay keystroke: a `command_invoke` APPROVAL hook returns `RequireApproval(ApprovalSpec(...))` with `require_human=True` and ONCE scope, then returns immediately. Core, not this extension, owns the durable ticket and reserved approval presentation (`docs/py/05-hooks.md` §2.6; `docs/py/06-policy.md` §“Approvals”; `docs/py/07-ui.md` §4.9).

The original's JSONL parsing, session-directory traversal, custom fuzzy index, executable UI factory, and extension-owned confirmation dialog are deleted. Resume, rename, and deletion execution are not faked: the frozen Python surface currently exposes no historical-session mutation verbs, so their explicit commands report that no session changed.

## Gaps

- `omp.sessions.resume(session_id)` (or an equivalent interactive-session switch verb) is absent. The complete frozen callable range in `crates/py/python/omp/sessions.py:168-200` contains only `current`, `list`, `usage`, and `journal`; `omp.agents.revive(ref)` at `crates/py/python/omp/agents.py:560-563` is explicitly for a parked child session, not a historical interactive session. `docs/py/09-journal.md` §“The sanctioned historical read API” documents no resume/switch operation.
- `omp.sessions.rename(session_id, title)` (or another durable title-update verb) is absent from `crates/py/python/omp/sessions.py:168-200`. `SessionInfo.title` is immutable index data at `crates/py/python/omp/sessions.py:97-116`, and `docs/py/09-journal.md` §“omp.SessionInfo” documents it only as an assigned title. The example can preview the immutable row replacement for picker logic but cannot persist it.
- `omp.sessions.delete(session_id)` is absent from `crates/py/python/omp/sessions.py:168-200`. The delete command is correctly gated by `RequireApproval`, but after approval there is no frozen invocation that can perform deletion; `docs/py/06-policy.md` §“Approvals” defines ticketing, not a session storage mutation. The example therefore never executes deletion.
- Frozen/docs divergence: `docs/py/09-journal.md` §§“async omp.sessions.get(session_id)” and “async omp.sessions.lineage(session_id)” specify `omp.sessions.get`, `omp.sessions.lineage`, `omp.SessionNotFound`, and `omp.SessionLink`, but none exists in `crates/py/python/omp/sessions.py:15-207` or its `__all__` at lines 203-207.
- Frozen/docs divergence: `docs/py/09-journal.md` §“omp.SessionInfo” types the row as `usage: omp.Usage` and `cost: omp.Cost`; the frozen row uses `Usage` and `UsageCost` at `crates/py/python/omp/sessions.py:67-116`, while `UsageCost` is omitted from both `sessions.__all__` at lines 203-207 and the top-level imports at `crates/py/python/omp/__init__.py:335-347`, and no `omp.Cost` exists.
