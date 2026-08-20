## What the pi original did

`@agnishc/edb-auto-name-session` replaced the label derived from a session's first message with a model-generated title after the first assistant response.

## The omp shape

This port observes `turn_end` and ignores tool-use boundaries, so generation begins only after the first settled response (`docs/py/05-hooks.md` §4.1). A typed `omp.state` entry at `StateScope.SESSION` claims the attempt before inference and makes every later turn a no-op; SESSION state is the session journal itself (`docs/py/09-journal.md` §`omp.StateScope`). The first projected user-message preview is bounded before it enters a choices-free `omp.agents.completion` request with `role="smol"`, a 24-token output ceiling, a deadline, attribution labels, and the completion service's session-budget admission (`docs/py/12-agents.md` §“One-shot completions”). No concrete model id is accepted: the auxiliary-lane rule keeps the call deprioritized and role-mapped by deployment (`docs/py/08-context.md` §“Background auxiliary inference”).

A fallback, exception, or empty emission leaves omp's generated title untouched and appends a typed failure outcome. Successful free text is collapsed and truncated to 72 characters before `omp.ui.set_title`; the frozen SetTitle-class effect performs final control-character sanitization and terminal title-stack handling (`docs/py/07-ui.md` §`ui.set_title`). There is no provider client, transcript mutation, hand-rolled retry, or terminal escape sequence.

## Gaps

None.
