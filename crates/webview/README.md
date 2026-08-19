# `omp-webview`

Modular embedded-browser surfaces. Follows [wry](https://github.com/tauri-apps/wry)'s
footsteps — embed real web content without shipping a browser engine — and
generalizes the part wry left as a TODO: the engine is pluggable, including
the browsers the user already has installed.

## Engines × surfaces

An **engine** renders web content; a **surface** is how that content reaches
the host.

| engine                       | protocol        | child | frames | window |
|------------------------------|-----------------|-------|--------|--------|
| `system` (WKWebView, macOS)  | in-process      | yes   | no     | no     |
| `chromium` (Chrome, Edge, …) | CDP             | no    | yes    | yes    |
| `firefox` (Gecko family)     | WebDriver BiDi  | no    | yes    | yes    |

- **child** — a native subview embedded in a host window at a `Rect`; the OS
  composites it above the host's rendering (wry's model, same airspace
  limitation).
- **frames** — the engine runs headless and streams RGBA8 frames the host
  composites itself (GPU texture, terminal images, …); input is forwarded
  explicitly with `WebView::input`. Chromium delivers compositor-paced
  screencast frames; Firefox has no screencast, so it is screenshot-polled
  (default 10 fps). Frames cross the automation socket compressed
  (`FrameFormat`: JPEG quality 80 by default — Chromium's own screencast
  default; PNG for pixel-exact needs) and are decoded straight to RGBA.
  Every delivered frame carries a client-side `damage` rect (tight diff vs.
  the previous frame) so hosts upload only what changed; unchanged captures
  are suppressed entirely. Firefox polling is dirty-driven: a preload script
  signals page changes, so a static page costs zero captures (1 Hz safety
  net for silent canvas/video changes).
- **window** — an engine-owned OS window (`chrome --app`-style; Firefox shows
  normal browser chrome).

## Structure

- `lib.rs` — `Engine` selection, `WebViewBuilder`, and the `WebView` facade
  dispatching over backends by enum (no `dyn`).
- `wk` — the in-process WKWebView child backend (macOS), delegate-driven
  events.
- `remote` — one driver thread per view (current-thread tokio runtime),
  flume command/event channels, ephemeral-by-default browsing profiles;
  `remote::chromium` speaks CDP, `remote::firefox` speaks BiDi, both over
  `remote::ws`.
- `discover` — installed-browser scan; `Engine::find(surface)` picks the best
  match and honors the `OMP_WEBVIEW_BROWSER` binary-path override.

## Philosophy

One event contract, one IPC contract (`window.ipc.postMessage`), one input
model — regardless of engine. Backends translate; hosts never see protocol
details. Remote engines never touch the user's daily profile: views get an
ephemeral profile unless a persistent one is configured explicitly.

Engine teardown is RAII: dropping a `WebView` closes the browser with a
bounded grace period. A host killed without running destructors (SIGKILL,
`process::exit`) orphans the engine process — drop views before exiting.

## Examples

```sh
cargo run -p omp-webview --example child  -- https://example.com   # WKWebView in a winit window
cargo run -p omp-webview --example frames -- https://example.com   # installed browser -> RGBA frames
OMP_WEBVIEW_BROWSER=/path/to/firefox cargo run -p omp-webview --example frames
```
