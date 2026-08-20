## What the pi original did

`@heyhuynhgiabuu/pi-pretty` took ownership of five complete Pi tool **names** — `read`, `bash`, `ls`, `find`, and `grep` — to obtain richer result presentation. The renderer therefore also became the executor. That is the total-name-capture mechanism documented in `docs/py/01-devices.md` around lines 104–135: presentation competed through load order, including the `pi-spark` collision archaeology that forced another extension to defer registration until `session_start` merely to discover who already owned `write`.

## The omp shape

This extension owns no tool name and registers zero tools. It owns only rendering for the explicit historical identities `read@1`, `bash@1`, and `grep@1`. The manifest routes those revision-keyed verdict entries, and `@omp.renderer(..., rev=1)` keeps the folds available when a later device revision becomes current. Package/layer selection is resolved from the manifest before import; a second fold for the same key is an import-time collision rather than a load-order winner.

The folds consume typed payload fields: read part values and blob sizes, bash status plus its reduced output counters, and grep file groups plus authoritative output accounting. They never split, regex, or otherwise reconstruct facts from model-facing prose. Tables use TML layout, nonzero shell exits receive error badges, and all byte counts come from typed bytes, UTF-8 values, or `BlobRef.size`. The bash reducer pre-computes stream totals; every `view` function is synchronous, deterministic, bounded by the verdict spill gate, and performs no filesystem, network, clock, or process I/O. This follows `docs/py/02-verdicts.md` §“Rendering: the update fold” and the pure 50 ms fold contract in `docs/py/07-ui.md` §4.13.

The deleted mechanisms are the point: no replacement `read`/`bash`/`ls`/`find`/`grep`, no borrowed incumbent descriptions, no `session_start` race probe, no renderer-side prose parsing, and no temporary output paths.

## Gaps

- `omp.DuplicateRenderer` is documented by `docs/py/02-verdicts.md` §“Rendering: the update fold” (lines 690–692 and the error table at 1197), but the frozen registry has no such public symbol and raises plain `ValueError` at `crates/py/python/omp/ui/__init__.py:604-605`.
