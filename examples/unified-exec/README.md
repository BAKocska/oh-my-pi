# What the pi original did

`pi-unified-exec` maintained long-lived shell sessions that the model could poll and drive with text, Ctrl-C, arrow keys, and other terminal input. That made PTY programs, REPLs, SSH clients, and development servers usable across tool calls.

# The omp shape

The soft `session` device has `open`, `poll`, `stdin`, `signal`, and `close` operations. `open` atomically ensures a namespaced Environment-owned process with a PTY, an explicit restart policy, and an optional `ReadyLog` probe that must pass before the tool returns. `stdin` translates named terminal keys to bytes and uses `Process.send`; signals and process-tree teardown use `Process.signal` and `Process.stop`. `poll` resumes from the caller's sequence cursor, stops after a bounded number of frames or a short quiet interval, and returns no already-consumed output. Its inline view is capped at 64 KiB; output beyond the requested cap is streamed into `omp.env.blobs` and returned as a `BlobRef` without accumulating an unbounded extension-side buffer.

The original hand-rolled PTY pool, subprocess spawning, output buffers, restart loop, and teardown handlers are deleted. The Environment owns all of them, following `docs/py/11-env.md` §“Named processes — omp.env.proc” (especially `proc.ensure` and `Process.output`/`send`/`signal`/`stop`) and §“Exec value types” for PTY/channel semantics.

# Gaps

- `omp.env.Pty` is documented at `docs/py/11-env.md:1029-1032`, but the frozen named-process signature exposes only `pty: object | None` at `crates/py/python/omp/env.py:1037-1047` and freezes no `Pty` class. This port sends the documented `rows`/`columns`/`terminal` wire shape through that object slot; a typed construction is unavailable.
