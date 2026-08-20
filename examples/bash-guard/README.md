## What the pi original did

`@shinynito/pi-menshen` provided a permission gate with an automatic secondary-model review mode. It bundled its own tree-sitter Bash parser and used deterministic command rules before asking the reviewer, with a rejection circuit breaker around that review path.

## The omp shape

The bundled parser, WASM payload, parse timeout, and shell-token heuristics are deleted: the shell core supplies one normalized `BashIR`, including `parse_ok`, `has_dynamic_eval`, and `is_read_only()` (`docs/py/06-policy.md` §4). PRECHECK contains only deterministic facts; its `Defer` is the phase-correct way for a proven read-only call to pass this policy, while parse failures and dynamic evaluation continue to REVIEW rather than being denied (`docs/py/05-hooks.md` §3.4). REVIEW uses the constrained `choices=("allow", "review", "deny")` ladder and a deterministic `default="review"` (`docs/py/12-agents.md` §One-shot completions). Rejection transitions move to OBSERVE before being durably appended with the frozen `omp.state.append(entry, *, scope=..., idempotency_key=...)` API at `StateScope.SESSION`; once three rejections trip the breaker, REVIEW becomes deny-free and OBSERVE journals one warning.

## Gaps

- No frozen-versus-documented signature divergence was encountered for `omp.state`: this port uses the frozen async `latest(kind, *, scope)` and `append(entry, *, scope, idempotency_key=None)` signatures directly.
