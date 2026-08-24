# What the pi original did

`@amitkot/pi-safe-github` exposed typed GitHub wrapper tools for pull requests, CI runs, issues, releases, and workflows (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`). Each operation occupied its own model tool schema and the package delegated GitHub access to `gh` CLI shell-outs.

# The omp shape

One soft `github` device replaces the five-plus per-operation schema slots. Its typed `path` dispatch covers `pr/list`, `pr/view`, `ci/runs`, `issue/list`, and `release/list`; `pr/comment` and `issue/comment` are declared mutation paths. The device is invoked through the `xd` builtin inside the core `shell` tool, retains the complete redacted JSON response in its typed verdict, projects at most twenty rows through `omp.Budget`, and surfaces GitHub rate-limit headers as a `RateLimit` field (`docs/py/01-devices.md` §§“The `xd` shell builtin” and “`@omp.device`”; `docs/py/02-verdicts.md` §§“`omp.Payload`” and “Projecting for the model”).

The `gh` child processes, CLI output parsing, environment-token lookup, and per-tool registrations are deleted. REST traffic uses `omp.env.http_get`/`http_post` under `env.net`, and authentication is a two-minute `omp.creds.mint_scoped("github-rest", provider="github")` token from the manifest-allowed credential store (`docs/py/11-env.md` §“Connection and capability”; `docs/py/13-inference.md` §§“Credentials: scoped, and secret-free by default” and “Credential scoping”). Neither projections nor typed faults include request headers or credential material; the retained response is recursively redacted against the scoped token before serialization.

Comments are not approved inside the device body. The `tool_call` APPROVAL hook immediately returns `RequireApproval(ApprovalSpec(..., require_human=True))` for either comment path, leaving the durable wait and one-call decision to Core (`docs/py/06-policy.md` §§“Approvals” and “`omp.ApprovalSpec`”). Read paths abstain.

# Gaps

- `omp.Device.subtool`: frozen `crates/py/python/omp/devices.py:290-296` returns a `ToolPath`, while `docs/py/01-devices.md` §“`omp.Device`” lines 903-922 specifies a child-device decorator (`@dev.subtool("create")`) with per-leaf schema, docs, and dispatch. Until those signatures agree, this example keeps one zero-slot `github` declaration and performs the same named sub-path dispatch through its typed `path` argument; the frozen layer cannot publish the documented `github/pr/list` child address for `xd github/pr/list …` without using a second registration mechanism.
