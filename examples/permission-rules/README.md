## What the pi original did

`@gotgenes/pi-permission-system` composed an 18-link authorizer chain that allowed, denied, deferred, or prompted for tool calls. It shipped its own shell path resolver and forwarded subagent approval requests through filesystem mailboxes to a parent session. It also attempted to coordinate admission decisions with enforcement outside the extension.

## The omp shape

This port keeps only the domain policy as a module-level tuple of frozen rule records: parse failures, dynamic evaluation, and unresolved `BashCommandIR.cwd` values deny; `BashIR.is_read_only()` identifies the allow tier; and `BashIR.writes_outside(ctx.roots)` identifies the ask tier. The PRECHECK hook is deterministic and fail-closed; because PRECHECK is deny-only, its allow match returns `Defer` so later policies retain their veto, as required by docs/py/05-hooks.md §§3.2 and 3.4. The separate APPROVAL hook immediately returns `RequireApproval(ApprovalSpec(...))`; Core, not the Python coroutine, owns and waits on the durable ticket (docs/py/05-hooks.md §4.1 and docs/py/06-policy.md §§Bash IR, Approvals, and Pattern 2).

The custom authorizer engine, AST walker, cwd folder, polling mailbox, and awaited dialog all disappear because structured shell facts and approval routing are ambient Core services. Kernel sandbox enforcement was ruled out of v1, so this extension is pure admission policy and does not claim to enforce effects after admission.

## Gaps

- `omp.BashIR` is not exported by the frozen layer yet (docs/py/06-policy.md §Bash IR).
- No frozen-vs-docs signature divergence is exercised beyond those missing symbols.
