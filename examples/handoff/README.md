## What the pi original did

`@noice-tech/pi-cutover` turned a completed planning conversation into a new implementation session with one `/handoff` command.

## The omp shape

`/handoff [objective]` folds only durable session truth: declared non-core journal entries become decision facts, and core settlement records become settled-outcome facts. It never reads `ContextView`, message items, previews, or transcript URLs. Each fact is UTF-8 bounded, the fold admits at most 128 facts, and the canonical typed `HandoffBrief` has a 256 KiB hard ceiling. The brief itself is journaled before delegation, so the parent retains an auditable input even if spawn is refused (docs/py/09-journal.md, “`omp.journal`”).

The child uses `Isolation.CLEAN`, `max_depth=0`, and a hard subtree `Budget` over requests, input/output tokens, spend, and wall time. This is deliberately not `Isolation.FORK`: FORK projects the parent's entire live chain into the child and charges that context on its first turn. It is also not summary injection into the parent: the command returns `ui.Consumed`, not `ui.Prompt`, while the child receives only the canonical brief as its task. Thus the planning parent remains settled and the implementation child starts with clean context (docs/py/12-agents.md, “Spawning”).

When the canonical brief exceeds `omp.journal.MAX_INLINE_BYTES`, the entry kind's `spill=True` gate stores it whole as an artifact. The child receives only a JSON pointer containing the resulting `artifact://` URL, parent session, and brief entry id; the URL is an ordinary resolvable read target. After spawn, `HandoffIssued` durably records the parent session, child session, child's typed `history://` transcript URL, brief entry, and optional artifact URL. If that link cannot land, the child is cancelled rather than leaving an unlinked implementation session (docs/py/09-journal.md, “Artifacts”; docs/py/12-agents.md, “The handle”).

## Gaps

- `omp.JournalEntry.artifact` is frozen as `object | None` (`crates/py/python/omp/journal.py:45-59`), while the docs specify `omp.ArtifactRef | None` with a typed `.url` and also document `omp.artifacts` (`docs/py/09-journal.md:464-478,892-920`). The frozen package exports neither `ArtifactRef` nor an `artifacts` namespace (`crates/py/python/omp/__init__.py:138-160,846-870`), so this example must validate the spilled record's documented `.url` dynamically.
- The docs define public `omp.CallOutcome` with four arms (`docs/py/02-verdicts.md:289-307`), but the frozen verdict exports contain only `Ok` and `Faulted` and no `CallOutcome`, `ArgsRejected`, or `Aborted` type (`crates/py/python/omp/_verdicts.py:54-73`; `crates/py/python/omp/__init__.py:138-160`). The fold therefore recognizes settled core records by journal kind plus the available decoded value class name rather than a complete frozen outcome union.
