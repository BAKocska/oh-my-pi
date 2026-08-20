## What the pi original did

`@robhowley/pi-yolo-seatbelt` intercepted Bash command text, ran ordered regular-expression rules, and either allowed, blocked, or synchronously prompted for risky commands. Its protected-path rule was `/(?:^|[\s"'=/])\.git(?=$|[\s"'`;|&()<>/=])/`. That inspected text rather than effects: `printf '.git/config\n' >> .gitignore` triggered the `.git` rule even though its only write target is the harmless `.gitignore` file, while a dynamic path could evade literal matching.

## The omp shape

This port reads `tier = "yolo" | "write" | "ask"` from `[settings]` and makes decisions only from `BashIR` facts. It never searches command text. `.git` protection examines components of inferred write `PathRef`s, and workspace confinement uses `BashIR.writes_outside(ctx.roots)`, including unresolved dynamic paths as outside (docs/py/06-policy.md §§Parse, not regex and Bash IR).

| BashIR fact | `yolo` | `write` | `ask` |
|---|---|---|---|
| `is_read_only()` | `Defer` | `Defer` | `Defer` |
| write inside a workspace root | `Defer` | `Defer` | `RequireApproval` |
| `writes_outside(ctx.roots)` | `Defer` | `Deny` | `RequireApproval` |
| inferred write targeting a `.git` path component | `Deny` | `Deny` | `Deny` |
| `has_dynamic_eval` (unless the `.git` rule already denied) | `Defer` with a journaled debug note | `Deny` | `RequireApproval` |

PRECHECK is deny-only: it returns `Deny` only for the tier's prohibited cases and otherwise abstains, preserving later vetoes (docs/py/05-hooks.md §§3.2–3.4). The ask tier is implemented separately in APPROVAL and immediately returns `RequireApproval(ApprovalSpec(...))`. The original awaited confirmation and its fifteen-minute suspended handler are deliberately deleted: Core owns one durable ticket, so no Python coroutine or extension-painted dialog waits on a person (docs/py/05-hooks.md §2.6; docs/py/06-policy.md §Approvals). Project or user configuration may pre-answer that ticket; the resulting receipt is identified as `ApprovalSource.CONFIG` (docs/py/06-policy.md §§Approval tiers and ApprovalDecision).

The command-text matcher, rule ordering engine, cached JSON configuration, logger, slash-command UI, and awaited `ui.confirm` all disappear. `[settings]` is the single configuration surface, BashIR is the path authority, and Core's approval journal is the single source of truth.

## Gaps

None.
