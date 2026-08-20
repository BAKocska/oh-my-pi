## What the pi original did

The `@piex-dev/hashline` extension replaced ordinary editing with a compact patch language whose operations were anchored to tagged source lines (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`). Its tags rejected edits authored against the wrong snapshot, but the replacement tool name did not give old calls a durable dialect identity.

## The omp shape

This port registers environment-placed `edit@patch.1` beside the built-in `edit@rep.1` and `edit@hl.1` families. The live arguments carry a patch envelope containing one path, the pinned content-revision tag, and sorted one-based inclusive line hunks. Execution opens one `omp.env` document lease, verifies the envelope tag against the lease revision, resolves the line hunks to frozen `omp.env.Edit` byte ranges, and commits through `Doc.edit` with rebase and best-effort formatting (`docs/py/11-env.md` §Document leases).

The projection always emits the patch-envelope grammar with the settled status, bounded by `PromptCaps`. The destination-owned `lift()` accepts both foreign built-in families directly: it validates the `rep.1` or `hl.1` argument schema, reads the successful dialect-neutral section outcome, and emits canonical `patch.1` arguments and verdict bytes with `LiftedCall.of`. It returns `None` for malformed arguments, non-success outcomes, multi-document outcomes, unsupported operations, or non-text content rather than inventing a migration (`docs/py/02-verdicts.md` §Revisions and the lift chain).

## Gaps

None — every symbol this port needs is frozen.

## Open question verdict

**Clique/direct pairwise lifts win; no canonical star emerged.** With three families, the live `patch.1` destination needs exactly two foreign-family lift implementations: `rep.1 -> patch.1` and `hl.1 -> patch.1`. At four families, the live destination would need three. If every family may become live, the complete directed pairwise graph contains 6 implementations at three families and 12 at four, versus 2 and 3 inbound edges for a canonical star. The smaller star count is not usable with the frozen walk: cross-family history jumps directly to the live destination, so a non-live canonical family cannot supply an intermediate hop. The two implemented arms both produced byte-identical lifted arguments and verdicts across repeated stub-smoke calls, showing that the existing destination-owned O(families) shape is sufficient without declaring a hub family (`docs/py/02-verdicts.md` open question 3).
