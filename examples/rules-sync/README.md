## What the pi original did

`@7n/rules` synchronized opinionated rules and skills into repositories, ran conformity commands, rewrote eligible shell calls through RTK, and exported completed runs for ADR capture. The extension was one executable arm of a larger repository-rules CLI.

## The omp shape

The port separates immutable content from effects. `rules_sync/skills/repository-rules/SKILL.md` is a `kind = "skills"`-class markdown declaration targeted at the `rules` prompt slot with `class = "stable"` and fixed priority. The manifest, not Python import order, fixes its placement. This follows `docs/py/08-context.md` §§“@omp.prompt_slot”, “omp.SlotClass”, and “Slot catalog”: rule contributions occupy the STABLE `rules` band, and deterministic priority and extension identity order them. Rule text is never Python, never passed to `eval`, and never interpreted as a shell script; a content reviewer can inspect the exact model-facing bytes without granting code execution. `docs/py/14-deploy.md` §3.2.3 supplies the zero-code integrity model for that content half.

The code half exposes one soft `rules_check` device. It accepts only declared check names, resolves their scripts from `[settings.commands]`, executes them through `omp.env.sh`, bounds output, and returns typed `CheckResult` and `Finding` values. It does not recreate RTK command rewriting: shell admission and transformation belong to the canonical hook procedure, while this device has no authority to rewrite unrelated calls (`docs/py/05-hooks.md` §§3.4 and “Call events”).

A `tool_result` OBSERVE hook journals successful edits or writes matching `[settings].decision_paths`. At the subsequent `agent_settled` domain boundary, the extension folds those typed touch entries since the previous `DecisionRecord`, appends one idempotent ADR-shaped typed entry, and returns `Settle()`. The journal is the only watermark and truth; no process cache or side file decides whether a record is pending (`docs/py/09-journal.md` §§“Durable-state consistency rules” and “@omp.entry_kind”).

## Gaps

- `omp.hook("agent_settled", phase=omp.HookPhase.OBSERVE)` cannot express the requested OBSERVE subscription. Frozen `crates/py/python/omp/hooks.py:301-312,344-347` classifies `agent_settled` as domain-return and rejects every explicit phase; `docs/py/05-hooks.md` §§“Turn and submission lifecycle” and “agent_settled is the goal-loop seam” agrees. This port therefore captures the decision in the real domain handler and returns `omp.agents.Settle()`.
- The authoring schema for a static `[[skills]]` content row inside a code-bearing extension is undocumented. `docs/py/14-deploy.md` §3.2.3 defines only top-level `kind = "skills"`, while `crates/py/python/omp/` exposes no skills declaration symbol; that documented kind forbids the `entry` and runtime code required by this port. The manifest records the intended hybrid content row explicitly, but the frozen host contract needs an exact schema before it can be claimed as wired.
