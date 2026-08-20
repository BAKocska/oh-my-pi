## What the pi original did

`pi-sandbox` sandboxed Bash execution and gated network, read, and write access with configurable allowlists and interactive permission prompts (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`, policy-safety cohort). Its prompt-to-grant loop could turn a denied operation into a broader runtime allowance.

## The omp shape

This port narrows that flow to enforced `FS_WRITE` or `FS_CREATE` violations for the exact `assistant-notes.jsonl` file below an `.omp/session-notes` directory. The `sandbox_violation` hook returns `Amend(scope=SESSION, retry=True, approval=...)`: its patch adds one non-recursive `PathRule` for the denied subject, while its `ApprovalSpec` binds the durable ticket, pattern, and evidence to that same subject and offers `ONCE` or `SESSION` scope. This is the documented widening exception rather than an extension widening on its own authority (`docs/py/06-policy.md` §§“`omp.Amend`”, “Approvals”, and “`omp.ApprovalSpec`”).

The `grant-widening` external approver accepts only a pending, single-reason ticket carrying that exact route, kind, scope set, subject-pattern equality, evidence marker, and note-path shape. It returns an `ApprovalDecision` with `source=EXTERNAL` and `scope=SESSION`; Core can therefore apply the exact widening as a standing session grant and retry the live denied command once. Every unrelated ticket is denied at `ONCE` scope. The frozen arm and ticket vocabulary are `Amend.approval`, `ApprovalDecision`, and `ApprovalTicket` (`crates/py/python/omp/policy.py:638-666`); the registered approver and imperative amend/decision operations are frozen at `crates/py/python/omp/policy.py:733-779`.

The original replacement Bash tool, extension-owned allowlist mutation, prompt painting, suspended wait, and sandbox reinitialization are deleted. Enforcement reports the structured violation, Core owns and persists the approval ticket, and Core alone composes the approved patch.

## Gaps

None — every symbol this port needs is frozen.
