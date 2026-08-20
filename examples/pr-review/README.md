# PR review

## What the pi original did

`pi-pr-review` coordinated parallel, model-tiered GitHub pull-request reviewers, validated their findings, rendered structured results, and published comments safely. Its reviewer-focus UI deliberately retained only bounded sanitized assistant status: it retained no objectives, diffs, tool arguments, tool results, or stderr (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:61`).

## The omp shape

The soft `review` device reads the configured review areas from `[settings]`. Each area becomes one `SubagentSpec` with its own explicit model alias, reasoning tier, strict JSON output schema, and finite subtree-wide `omp.agents.Budget`; the complete wave is admitted atomically in one `spawn_all` call. Reviewers start with `Isolation.CLEAN`, cannot spawn descendants, and return only `{findings: [...]}`. The schema fixes the area and validates severity, path, line, and rationale. Strict mode fails malformed child output, and the parent validates every row again before constructing `Finding`; rejected rows never reach the report or publisher (`docs/py/12-agents.md` §§“Spawning” and “The handle”, especially `SubagentSpec.output_schema`, `schema_mode`, and `SubagentResult.data`).

The merge diff comes from bounded `omp.env.sh.run("git diff …")` output. Environment capping preserves an oversized command result in `Completed.artifact`; if the extension's smaller 24,000-byte reviewer-preview ceiling is crossed first, it stores the full bytes with `omp.env.blobs.put` and passes only the prefix to children. The report retains the `BlobRef` and full byte length rather than copying the large diff through Python or inventing a truncation file (`docs/py/11-env.md` §§“Exec — `omp.env.sh`” and “`Completed`”, lines 967–1053; §“Blobs”, lines 1209–1270).

While the wave runs, a retained right rail folds only `SubagentHandle.progress()` into bounded rows: declared area, enum status, and one control-stripped 80-character activity line. The rail function has no parameter for the reviewer objective, diff, model arguments, raw output, tool results, or stderr, preserving the original's deliberate non-retention property. It is unmounted on every exit and never enters scrollback (`docs/py/12-agents.md:498-504`; `docs/py/07-ui.md` §§“Rails” and “Identity, sizing, chrome”).

Comment publication is the separate `review/publish` child device. It accepts only validated findings from configured areas, formats one byte-bounded comment, and invokes `gh` through `omp.env.sh`. A `tool_call` APPROVAL hook immediately returns `RequireApproval(ApprovalSpec(..., require_human=True))`; Core files and owns the durable ticket, and no Python coroutine or extension-painted dialog waits on the human (`docs/py/05-hooks.md` §“`omp.RequireApproval`”; `docs/py/06-policy.md` §“Approvals”).

Deleted mechanisms: per-reviewer CLI orchestration, unbounded live transcripts, diff/argument/result/stderr retention in UI state, extension-owned approval waits, raw subprocesses, and ad hoc temporary spill files.

## Gaps

- `omp.Device.subtool`: the frozen signature accepts only `name` and therefore always inherits the parent's `place`, `precedence`, `tier`, intents, and effects (`crates/py/python/omp/devices.py:362-410`). The documented signature likewise shows only `subtool(name: str)` (`docs/py/01-devices.md:911`), but the same section then says a child inherits those fields “unless the `subtool` call overrides them” and that every child carries its own schema (`docs/py/01-devices.md:927-929`). Those override arguments do not exist. This port needs no unsafe workaround—the APPROVAL hook gates `review/publish` independently—but the prose describes an unavailable API.
