## What the pi original did

`@mrclrchtr/supi-bash-timeout` applied configurable default and maximum timeouts to bash calls so a stalled command could not hang a session. It rewrote the complete tool-call input before execution while preserving shorter explicit timeouts.

## The omp shape

This port keeps the focused maximum-timeout policy from docs/py/03-params.md §4: a TRANSFORM hook replaces only an excessive canonical `omp.Duration`, while a missing or shorter timeout is left untouched. The explicit transform order and declarative bash-only filter follow docs/py/05-hooks.md §§3.1, 3.4, and 3.6, deleting manual target dispatch and whole-argument mutation.

## Gaps

