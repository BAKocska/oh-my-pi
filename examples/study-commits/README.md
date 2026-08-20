# What the pi original did

[`@anthnykr/pi-study-commits`](https://www.npmjs.com/package/@anthnykr/pi-study-commits) let a user choose recent Git commits, fetched their diffs, and pasted that material through pi's clipboard/editor workflow so it could be added to the conversation (`catalog.md:462`).

# The omp shape

`/commits` runs a 50-row, NUL-delimited `git log` through `omp.env.sh.run`, then presents one native `ui.multi_select` overlay. Each `ui.SelectItem.cells` tuple is `sha · date · subject`, so the host's shared table solver aligns and shrinks the columns rather than the extension drawing terminal rows (`docs/py/07-ui.md` §4.10, lines 1250–1264; `docs/py/11-env.md` §Exec, lines 930–970).

The selected object ids are checked against the bounded log result and passed together to one `git show`. Exactly one `omp.agents.inject(..., mode=DeliveryMode.NEXT_TURN, visible=False, role="system")` call queues the result as a system notification at the Core's `TurnBoundary`; it cannot interrupt an in-flight tool batch (`docs/py/12-agents.md` §`omp.agents.inject`, lines 816–827, and §`DeliveryMode`, lines 1102–1112).

The clipboard-paste workflow is deleted. The command never reads or writes the clipboard, mutates the composer, or makes the user manually submit generated text. If the complete diff plus its notification header exceeds `omp.SPILL_INLINE_LIMIT`, the diff bytes go to `omp.env.blobs.put` and the one injected notification carries the resulting `BlobRef` digest and byte length instead. A `Completed.artifact` produced by the Environment's earlier central output gate is forwarded the same way, so oversized output is referenced whole and never silently truncated (`docs/py/02-verdicts.md` §“The spill gate”, lines 1017–1083; `docs/py/11-env.md` lines 1044–1052 and §Blobs, lines 1196–1271).

# Gaps

None.
