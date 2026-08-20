# Shell hooks

## What the pi original did

`@hsingjui/pi-hooks` loaded Claude Code-style command hooks and translated Pi lifecycle, compaction, tool, prompt, and stop events into JSON written to each command's standard input. It supported the nine command-hook names below and let command results block selected pre-event paths.

## The omp shape

`[settings]` is an event-name map. Each value has a non-empty `command`, a duration-string `timeout` (default `5s`), and `on_failure = "continue" | "deny"` (default `continue`):

```toml
[settings]
Stop = { command = "cat >/dev/null", timeout = "2s", on_failure = "continue" }
PreToolUse = { command = "./policy/check-tool", timeout = "3s", on_failure = "deny" }
```

Every configured command receives one newline-terminated JSON object on stdin. Payloads retain the Claude Code common names (`session_id`, `cwd` when present, `hook_event_name`) and the relevant event fields, plus `omp_event_name` and a bounded `omp_event` projection. Input is capped at 64 KiB. Commands run through the Environment-owned shell with the configured timeout clamped to the callback's remaining harness deadline (`docs/py/11-env.md` §“`omp.env.sh` — guarded command execution”; `docs/py/00-overview.md` §“Timeouts and cancellation”).

| Claude Code hook | omp event | Mode |
|---|---|---|
| `SessionStart` | `session_start` | OBSERVE by default; PRECHECK when `on_failure = "deny"` |
| `SessionEnd` | `session_shutdown` | OBSERVE |
| `PreCompact` | `compaction` | Domain callback returning `None`, the frozen equivalent of non-gating observation |
| `PostCompact` | `compaction_done` | OBSERVE |
| `PreToolUse` | `tool_call` | OBSERVE by default; PRECHECK when `on_failure = "deny"` |
| `PostToolUse` | `tool_result` where `outcome == OK` | OBSERVE |
| `PostToolUseFailure` | `tool_result` where `outcome != OK` | OBSERVE |
| `UserPromptSubmit` | `user_input` | OBSERVE by default; PRECHECK when `on_failure = "deny"` |
| `Stop` | `agent_end` | OBSERVE |

Gating is off by default because user shell snippets are automation, not policy: an absent, broken, or slow observer must not stop the agent. `on_failure = "deny"` is accepted only for the three mappings with a real PRECHECK seam. Those handlers return only `Defer` or `Deny`, and their manifest rows are explicitly `failure = "fail-closed"` as required by the hook failure procedure (`docs/py/05-hooks.md` §§2.4, 3.2, and 3.13). Post-tool hooks never gate or rewrite a landed result; an `Ok` remains immutable. `SessionEnd`, compaction completion, failed-result reporting, and `Stop` therefore reject `on_failure = "deny"` during activation rather than pretending they can block.

Pi's in-process extension runner, direct process spawning, mutable result patches, and ad-hoc hook timeouts are deleted. `omp.env.sh` owns the process tree and bounded output; OBSERVE is fail-open; PRECHECK is deny-only; the journal remains the sole durable truth. Activation validates the entire settings map, and `ShellHooksConfigError` carries `code`, `event`, `field`, and `detail`, so an unknown hook name cannot silently become a dead subscription.

## Gaps

- `omp.hook("compaction", phase=...)` is self-contradictory in the docs: frozen `crates/py/python/omp/hooks.py:301-312,344-347` classifies `compaction` as a domain event and rejects every phase, matching `docs/py/08-context.md` §2's `CompactionVerdict` examples, while `docs/py/05-hooks.md:1787` lists it as phased `HookDecision` with “any” phase. This port uses the frozen domain form and returns `None`.
- `docs/py/11-env.md:991-997` documents `await Run.stdin(data)`, but frozen `crates/py/python/omp/env.py:742-748` exposes `Run.write(data)` plus `Run.eof()` and has no `stdin` symbol. This port uses the frozen `write` signature.
