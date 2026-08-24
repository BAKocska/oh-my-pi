# Computer use

## What the pi original did

`@amaster.ai/pi-computer-use` lazy-started a precompiled Rust driver and registered 49 version-pinned MCP tools for GUI windows, mouse, keyboard, screenshots, and accessibility operations (`catalog.md:194`).

## The omp shape

The manifest declares one soft `computer` parent whose fixed `screenshot`, `click`, `type`, `scroll`, and `window/*` leaves are reached through shell commands such as `xd computer/screenshot`. The slot math is therefore **49 pi tools → 1 omp catalog device → 0 model schema slots**; schemas and CLI usage are fetched with `xd computer/<leaf> --help` (`docs/py/00-overview.md` §2; `docs/py/01-devices.md` §“The `xd` shell builtin”).

The native stdio driver boots inside the manifest-declared local `worker:driver`. Its warm process state survives calls, while a native crash is confined to that supervised worker rather than the session (`docs/py/04-placement.md` §4, “Native-crash isolation”). Click, type, scroll, and window mutations yield progressive `Update` frames followed by exactly one `Done`, using the `AsyncIterator[Update | Done]` body contract (`docs/py/01-devices.md` §“One body contract”). Screenshots cross the worker boundary as `Spill` bytes that become a `BlobRef` inside a `BlobPart`; the renderer hands that reference to `ui.image`, never placing base64 or a temporary path in prose (`docs/py/04-placement.md` §“Large payloads”; `docs/py/02-verdicts.md` §`omp.BlobPart`).

Deleted mechanisms: 49 eager MCP schema slots, harness-owned native-process lifecycle, inline base64 screenshot text, temporary screenshot paths, and bespoke crash recovery.

## Gaps

None — every symbol this port needs is frozen.
