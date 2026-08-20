## What the pi original did

`avtc-pi-parallel-work-guardrail` blocked or prompted for agent-issued Git operations that could disturb other agents working in parallel. It distinguished the shared checkout from linked worktrees and automatically applied a configured answer when its prompt timed out.

## The omp shape

This port classifies normalized `BashCommandIR.argv` values instead of scanning shell text. On the primary checkout, `checkout`, `rebase`, `reset`, destructive `clean`, and forced `branch -D` are disruptive; `clean --dry-run` and unrelated Git commands pass. Ordinary operations in a linked worktree pass, while explicit `-C`, `--git-dir`, or `--work-tree` targeting remains conservative. The topology check uses `omp.env.fs.lstat()` on the Environment-scoped `.git` marker: a directory is the primary shared checkout and a regular file is a linked worktree. It never shells out to Git.

PRECHECK performs only classification and returns `Defer`, preserving the deny-only phase contract (`docs/py/05-hooks.md` §§3.2–3.4). APPROVAL repeats the current topology read and immediately returns `RequireApproval(ApprovalSpec(...))`. Both `timeout` and `default_on_timeout` are copied from `[settings]` into `ApprovalSpec.timeout` and `.default`, so Core owns the deadline, durable wait, and timeout decision (`docs/py/06-policy.md` §§Approvals and `omp.ApprovalSpec`). There is no Python timer, suspended prompt coroutine, or extension-owned ticket state.

## Gaps

- `omp.env.PathMeta` and `omp.env.FileKind` are documented as the typed return and discriminator for `fs.lstat` in `docs/py/11-env.md` §Raw filesystem value types (lines 908–922), but neither symbol exists in frozen `crates/py/python/omp/env.py`; `_Fs.lstat` instead returns `Any` at lines 600–602. This port normalizes the backend receipt's documented `kind` field until the frozen layer exports those types.
- A first-class `omp.env.worktree` topology query is absent: frozen `crates/py/python/omp/env.py:49-50` exposes only `Capability.WORKTREE`, while `docs/py/11-env.md` §Connection and capability (lines 354–365) says the worktree capability is grantable but unimplemented and covers isolated creation/destruction/merge, not topology. The `.git` `lstat` fallback is Environment-routed and remote-safe, but the missing topology symbol remains a frozen-layer gap.
