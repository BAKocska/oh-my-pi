## What the pi original did

`pi-extensible-workflows` and `pi-workflow-engine` orchestrated parallel subagents, typed handoffs, synthesis steps, resumable runs, and optional worktrees. The former rebuilt pi's runtime around each step, executed user JavaScript in a persistent QuickJS sandbox, launched a separate pi CLI process and `git worktree` command per spawn, hand-rolled whole-wave preflight, and persisted authoritative run JSON under `.pi/workflows/runs/`.

## The omp shape

This port follows `docs/py/12-agents.md` §§“Spawning”, “The handle”, and Pattern 2, “pi-extensible-workflows — a DAG whose fan-out does not cost a process each”. The soft `workflow` tool accepts `WorkflowNode` declarations plus dependency edges directly, or decodes the same DAG from the manifest's JSON `workflow` setting. Each node compiles to a background `SubagentSpec` with its own hard `Budget`; worktree nodes request a Core-owned patch outcome rather than shelling out to Git.

A deterministic topological scheduler submits every runnable wave in one `spawn_all` call, so validation and admission are whole-wave or none. It waits for Core-owned background settlement and immediately journals a typed terminal receipt. Downstream prompts contain only durable `agent://` output references. A failed, cancelled, or exhausted node does not stop an independent branch; only its descendants receive typed `WorkflowNodeSkipped` journal entries. Re-running the same named DAG fingerprints the declaration, folds settled and skipped entries from the live journal, and starts only work absent from that fold. There is no workflow state file, per-spawn CLI invocation, QuickJS sandbox, reconstructed runtime, Git subprocess, or hand-written preflight. Journal and artifact semantics are those of `docs/py/09-journal.md` §§“Typed extension entries” and “URL namespace”.

The configured form is a JSON object such as `{"name":"review","nodes":[{"name":"scan","task":"Scan the tree"},{"name":"report","task":"Synthesize the findings"}],"edges":[{"upstream":"scan","downstream":"report"}]}` supplied through `[extensions.settings."examples.workflows"]` as `workflow = '''...'''`.

## Gaps

None.
