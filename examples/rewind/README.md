## What the pi original did

`@ayulab/pi-rewind` maintained a bare shadow repository under `~/.pi/checkpoints/`, staged the whole work tree into private refs on every turn, intercepted session-tree navigation, and force-restored files. Its fixed checkpoint cap required manual ref pruning and its writes bypassed the document authority.

## The omp shape

This port exposes two soft tools. `checkpoint` asks the environment for a content-addressed workspace snapshot and journals a typed `Checkpoint` pairing the blob-manifest generation hash with the physical transcript event index. `rewind` folds those entries to the newest live checkpoint at or before its target and translates `conversation`, `workspace`, and `both` to `omp.agents.RestoreScope`; every apply is preceded by a real `dry_run` call. For a workspace apply, the code rejects any completed report lacking `RestoreReport.undo_snapshot_id`, making the unconditional pre-restore undo generation an enforced invariant rather than an ignored receipt. This follows `docs/py/12-agents.md` §Time travel and §`crates/env` + `crates/app/src/envd` — workspace generations.

There is no shadow repository, private ref, pruning routine, checkpoint cap, side file, or cache. Snapshot manifests are retained by journal reachability and collected by the central mark-and-sweep described in `docs/py/09-journal.md` §Reachability is the retention rule. Per the 2026-08-19 ruling, and the detached-job rule in `docs/py/12-agents.md` §Time travel, a rewind never cancels background children; an applied rewind appends a typed `BackgroundChildrenWarning` so their later settlement is not mistaken for a resurrected child.

## Gaps

- `omp.agents.RestoreScope`, `omp.agents.RewindPending`, `omp.agents.rewind`, and `omp.agents.snapshot` are documented in `docs/py/12-agents.md` §Time travel but are absent from `crates/py/python/omp/agents.py` and its `__all__`. The example imports the documented symbols with an inline GAP marker and cannot execute against the frozen package until that module exports and wires them.
- The documented generation vocabulary diverges internally: `docs/py/12-agents.md` §Time travel defines `Snapshot.generation: int` as monotonic, while its §`crates/env` + `crates/app/src/envd` — workspace generations and `docs/py/11-env.md` §Closing the remaining gaps define a generation as the blob-manifest hash. This port follows the requested storage invariant and journals `Snapshot.id`, the documented content-addressed manifest identifier.
