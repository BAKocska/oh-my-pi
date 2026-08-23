# omp-chat-ui

`omp-chat-ui` is the host-agnostic designed chat scene shared by omp frontends. It owns the ordered block scheduler, bounded live presentations, composer attachments and completion, status chrome, viewport-local damage, and matching overlays. It does not own an agent, persistence, credentials, catalog, terminal, or synthetic demo data.

Every conversation item receives a monotonic `BlockOrdinal`. Streaming assistants and running tools remain live blocks whose scheduler-owned height allocation is clipped inside the fixed viewport. Finalization freezes an immutable semantic `Entry` snapshot and hides the live presentation; snapshots remain indexed by ordinal until explicit retirement succeeds. A later block may finish first, but it cannot retire across an earlier active block.

A host creates `Chat` with its `UiContext`, forwards input as `Intent` values, and applies `BackendEvent` values through `Chat::apply_backend_event` or the typed mutation methods. `Chat::render` returns an exactly viewport-sized `ViewportFrame`; painting it is history-neutral. On each normal-screen frame tick, the host asks `Chat::retirement_batch(width)` for the maximal contiguous finalized prefix, passes that frame to `Renderer::retire`, and calls `Chat::mark_committed(range.end)` only after the terminal transaction succeeds. A zero-height batch is valid when its ordinal range contains only dropped tombstones.

Native scrollback changes only through explicit retirement. Viewport damage is merely a paint optimization and never implies that rows are stable, committed, or safe to scroll. Rewind and clear operations leave committed native rows immutable, tombstone affected uncommitted snapshots, append a finalized semantic marker, and preserve a contiguous retirement frontier.

The production terminal host is `omp-app`. Other frontends can share the same scene and snapshot formatter without inferring terminal history from presentation geometry.
