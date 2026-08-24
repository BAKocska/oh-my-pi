# Knowledge index

## What the pi original did

`@galvinsan/pi-mentis-knowledge` indexed files, repositories, packages, documentation URLs, and common office/archive documents into persistent dense-plus-full-text storage. It exposed two model tools: asynchronous durable ingestion and later hybrid retrieval.

## The omp shape

`ingest` and `search` are ordinary devices reached through the `xd` builtin inside the core `shell` tool (`xd ingest …` and `xd search …`; each command's exact flags are available via `--help`). Both bodies use `place="env"`: ingestion streams the shared, gitignore-aware `omp.env.find.walk()` view and reads documents through `omp.env.docs`, while search opens `knowledge.sqlite` below `await omp.state_dir()`. Source documents remain truth; the SQLite FTS5 rows and deterministic hash vectors are replaceable derived data. Deleting the database and ingesting the same roots reconstructs equivalent search results.

The intended ingest call hands `_ingestion_frames()` to the host's detached-call dispatcher and returns its `omp.Detached` item immediately. The Environment-owned job emits ephemeral `omp.Update` frames while walking and committing, then yields one `omp.Done(Ingested(...))`; `JobBoard` delivers that settlement as a later `TurnBoundary` interrupt rather than extending the original tool turn. This follows `docs/py/03-params.md:149-179`, `docs/py/12-agents.md:1885-1893`, and the settled-turn mailbox order at `docs/py/12-agents.md:99-120`.

The original in-process blocking ingest, private filesystem walker, plugin-owned background task, and database-as-authority are deleted. There is no hand-rolled timer, daemon, subprocess, or socket protocol.

## Gaps

None — every symbol this port needs is frozen.
