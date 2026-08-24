## What the pi original did

`@danypops/pi-papyrus` connected Pi to a separately running Papyrus daemon and graph-database process. It stored tasks, rules, notes, playbooks, and graph links there, exposed interactive graph views, and rewrote the agent system prompt with the currently active tasks and rules.

## The omp shape

`Task` and `Rule` are typed, versioned journal entry kinds, so task-node mutations, prerequisite links, and rule state remain durable session truth. The JSON adjacency file under `await omp.state_dir()` is only a query index: its watermark is compared with the journal, malformed or stale bytes are discarded, and replay deterministically reconstructs nodes and edges (docs/py/09-journal.md §“@omp.entry_kind”; §“omp.state_dir”; especially the journal-versus-index rule at lines 123-159). Deleting that file loses no facts.

The soft `graph` device declares `task/add`, `task/link`, `task/next`, and `rule/list` as real child paths with `@graph.subtool(...)`; callers therefore dispatch through the ordinary `xd` shell builtin rather than a second CLI or RPC surface (docs/py/01-devices.md §“omp.Device”, `subtool(name: str)`). `task/next` queries the adjacency index, while every mutation is appended first and then rebuilds the index from journal replay. Cyclic links are rejected before the durable append.

The `rules` prompt contribution renders a preloaded immutable snapshot, includes only active `Rule` values, sorts them by stable rule id, normalizes each rule to one line, and admits only complete lines within `PromptContext.budget_bytes`; tasks remain queryable graph data rather than turn-volatile prompt text. It is explicitly `SlotClass.STABLE`: docs/py/08-context.md §“Prompt slots and prefix stability” lines 118-153 requires semantic band ordering, and §“omp.SlotClass” defines STABLE content as changing only on an explicit user-observable event. Consequently an unchanged rule set produces byte-identical slot output rather than churning the prefix cache. The activation hook primes that snapshot before the first prompt and rebuilds the disposable index after restart.

The Papyrus daemon, its graph-database process, shared RPC protocol, CLI framework, and prompt-string rewriting are deleted. Agent Core owns durable journal truth, the Environment owns only the rebuildable index file, `xd` owns device dispatch inside the shell, and the prompt assembler owns ordering and cache breakpoints.

## Gaps

- `omp.Device.subtool`: frozen `crates/py/python/omp/devices.py:362-423` accepts only `name: str`, matching the signature printed at docs/py/01-devices.md:911, but the same docs at lines 927-929 say a `subtool` call can override inherited `family`, `place`, `precedence`, and `tier`; no such keyword parameters exist.
- `omp.ExtensionActivateEvent`: frozen `crates/py/python/omp/events.py:748-749` names the activation payload `ExtensionActivateEvent`, while the rebuild worked example at docs/py/09-journal.md:1296-1298 annotates the nonexistent `omp.ExtensionActivate`.
