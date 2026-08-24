# Subagent swarms

## What the pi original did

The 17-package family identified in `docs/py/12-agents.md` — including `@tintinweb/pi-subagents`, `@narumitw/pi-subagents`, `pi-crew`, `pi-extensible-workflows`, `pi-background-tasks`, and `pi-workflow-engine` — delegated work by launching the pi CLI as child processes. The catalog describes their common parallel/background execution, steering, result retrieval, model routing, workflow fan-out, and live roster surfaces (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:31-64, 227, 265-276, 314-316, 374`).

## The omp shape

One soft `swarm` device is discovered and invoked only through the `xd` shell builtin (`xd swarm …`). `run` constructs every `SubagentSpec` with a finite hard `Budget` and sends the entire wave in one `spawn_all` CONTROL frame. The Core validates every spec before starting any child and admits or queues the wave as a whole; this replaces the explicit “preflight validation” that `pi-extensible-workflows` needed to prevent partial CLI fan-out (`docs/py/12-agents.md`, “Spawning” and Pattern 2). Each budget is recursively clamped to every ancestor's unspent remainder, so fan-out cannot widen the root ceiling.

`status` reads the Core roster, `steer` uses the child mailbox and returns `AgentGone.transcript_url` when the target is already terminal, and `harvest` waits for terminal results while returning `SubagentResult.subtree_usage` rather than node-only usage (`docs/py/12-agents.md`, “The handle” and “Listing, revival, and limits”). Members marked `detached` carry `background=True`; the Core's `JobBoard` owns them and posts their settlements as `InterruptSource::Job` mailbox items, so the invoking tool may return while they continue (`docs/py/12-agents.md`, “What already exists to build on”).

Deleted mechanisms:

- no CLI child-process spawning;
- no stdin-based steering;
- no Unix-socket approval server.

## Gaps

- The frozen module still lacks the documented `SubagentHandle`, `SubagentResult`, `RunStatus`, `AgentRef`, and `Receipt` types needed to type the returned live objects.
- `omp.agents.SubagentSpec.budget` is documented in `docs/py/12-agents.md`, “Spawning” (`SubagentSpec` field table), but the frozen dataclass in `crates/py/python/omp/agents.py:174-185` has no `budget` field. The example imports the documented symbols and marks each dependency with `GAP` rather than creating a local substitute.
- `omp.agents.Usage` is documented in `docs/py/12-agents.md`, “The handle”, with `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_tokens`, `cache_write_tokens`, `requests`, `cost_usd`, and `wall`; the frozen `crates/py/python/omp/agents.py:14-23` instead exports `input`, `output`, `cache_read`, `cache_write`, `reasoning`, and `cost_usd`. `harvest` therefore passes `subtree_usage` through without inventing a competing projection.
