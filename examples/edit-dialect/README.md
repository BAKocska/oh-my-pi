## What the pi original did

`pi-hashline-edit-pro` disabled the built-in `edit` tool at session start, then registered hash-anchored read, replace, and undo tools. It kept its own line-hash and undo state, directly mutated files, and rewrote other tools' formatted results to show hashes and diffs.

## The omp shape

This port declares `edit@hlx.1` as a soft, environment-placed dialect beside core `edit@hl.1`; it never calls `setActiveTools`, disables the core tool, or captures `edit` by registration order. That machinery is deleted because `family`/`rev` identify both dialects durably and `caps.dialect` selects the projection appropriate to the live model, while soft intent keeps the extension behind the `xd` builtin inside the core `shell` tool rather than taking another schema slot (`docs/py/01-devices.md` §“pi-hashline-edit-pro → a second dialect, not a deleted tool”; `docs/py/02-verdicts.md` §3).

The `hl.1` lift deterministically rewrites the old `{“input”: ...}` argument bytes as canonical `{“patch”: ...}` bytes and retains the dialect-neutral verdict bytes exactly. Execution opens one `omp.env.docs` lease and commits through `Doc.hashline`, so the mutation uses the document authority's pinned revision and compare-and-swap path instead of private hash state or direct filesystem writes (`docs/py/11-env.md` §Document leases).

## Gaps

- `Doc.dry_run` and typed hashline edit-result values are not frozen yet, so this port cannot emit a dry-run preview or statically check those result fields.
