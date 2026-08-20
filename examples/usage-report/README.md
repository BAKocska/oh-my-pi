# Usage report

## What the pi original did

`@tmustier/pi-usage-extension` built usage dashboards by walking `~/.pi/agent/sessions/**/*.jsonl`, checking file size and mtime against its own cache, and parsing session records itself. Its centerpiece optimization scanned only each line's first 1,024 bytes for four hard-coded byte prefixes, with a separate hand parser for tool results larger than 64 KB. It then recursively discovered child sessions, maintained a SQLite mirror, and grouped token cost, models, projects, and trends from those reconstructed records.

## The omp shape

The soft `usage_report` device calls `omp.sessions.list()` and `omp.sessions.usage()` only. Its `group_by` argument selects day series or model/project groups, while `days`, `project`, and `max_rows` bound the indexed query and projection. The result is durable structured data rendered one whole table row at a time through `omp.Budget`; no session path is opened and no JSONL, byte prefix, mtime, cache, or child-tool result is parsed. The statusline uses the round-1 keyed `omp.ui.set_status` convention and runs the same sessions usage query from local midnight to show today's spend (`docs/py/09-journal.md` §1, “a disk crawler becomes a query”; `docs/py/10-telemetry.md` §3).

The billing-truth reversal is deliberate: spend comes from the core's durable per-turn journal receipts through `omp.sessions.usage`, never from the at-most-once telemetry firehose. Telemetry analytics may have silent gaps and therefore cannot be accounting truth (`docs/py/10-telemetry.md`, “The firehose is droppable by contract,” especially the billing-truth reversal, and §3 lines 1857–1861).

## Gaps

None. The frozen `omp.sessions` list/usage query, bounded verdict projection, hooks, and keyed UI status surface cover this port without filesystem session access.
