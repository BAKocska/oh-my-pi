# `omp-desktop`

Native desktop capture, input, and accessibility automation for macOS, Linux,
and Windows. `DesktopSession` exposes one asynchronous interface for listing
screens and windows, capturing targets, driving pointer and keyboard input,
and inspecting or acting on native accessibility trees.

## Structure

- `lib.rs` owns the session facade and its dedicated actor thread. The actor
  serializes every platform operation, lazily creates the selected backend,
  and shuts down with a bounded wait.
- `types.rs` defines capture targets, display and window geometry, capability
  reports, input options, and accessibility snapshots and nodes.
- `backend.rs` is the internal platform contract for capture, input, window
  activation, and accessibility operations.
- `frame.rs` applies capture size caps, encodes captures as PNG, and preserves
  the geometry needed to translate capture-relative input coordinates.
- `ax.rs` builds bounded snapshots and queries while keeping opaque native
  accessibility handles behind generation-fenced references.
- `macos`, `linux`, and `win32` implement the native backends. Linux supports
  X11 directly and Wayland through portal capture and libei input, with
  PipeWire capture available through the `wayland-pipewire` feature.

## Philosophy

Platform objects stay on one actor thread rather than crossing async tasks.
A successful capture establishes the coordinate frame used by later pointer
operations for that target, so scaled and multi-display images remain aligned
with native desktop coordinates. Window IDs and accessibility references are
opaque, backend-owned capabilities are reported explicitly, and permission or
backend failures are returned as typed `DesktopError` values rather than
silently degrading an operation.
