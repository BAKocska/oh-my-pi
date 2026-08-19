//! Events emitted by a [`WebView`](crate::WebView), delivered over a flume
//! channel so hosts can consume them from any thread.

use std::sync::Arc;

use bytes::Bytes;
use omp_core::Str;
use parking_lot::Mutex;

/// A single decoded frame from a
/// [`frames`](crate::WebViewBuilder::build_frames) surface: tightly packed
/// RGBA8 rows, ready to upload as a texture.
#[derive(Clone, Debug)]
pub struct Frame {
	/// Frame width in device pixels.
	pub width:  u32,
	/// Frame height in device pixels.
	pub height: u32,
	/// `width * height * 4` bytes of RGBA8 pixel data (O(1) to clone).
	pub data:   Bytes,
	/// Bounding box `[x, y, w, h]` (device px) of everything that changed
	/// since the *previously delivered* frame; the full frame on first
	/// delivery and after a resize.
	///
	/// `data` always holds the complete frame — `damage` is an upload hint:
	/// a host keeping the previous frame in a texture may upload only this
	/// region. Skipping frames requires uploading the union of the skipped
	/// frames' damage.
	pub damage: [u32; 4],
}

/// Something happened inside the web surface.
#[derive(Debug)]
#[non_exhaustive]
pub enum WebViewEvent {
	/// A navigation committed; carries the new URL.
	Navigated(Str),
	/// The document title changed.
	TitleChanged(Str),
	/// A page began loading; carries the URL.
	LoadStarted(Str),
	/// The page finished loading; carries the URL.
	LoadFinished(Str),
	/// The page called `window.ipc.postMessage(...)`; carries the payload.
	Ipc(Str),
	/// A new frame is available (frames surfaces only).
	Frame(Frame),
	/// The engine terminated abnormally; carries a diagnostic.
	Crashed(Str),
	/// The engine or page is gone (window closed, process exited).
	Closed,
}

/// Mutable per-view state kept current by backend delegates/event streams so
/// [`WebView::url`](crate::WebView::url) and
/// [`WebView::title`](crate::WebView::title) answer synchronously from any
/// thread.
#[derive(Debug, Default)]
pub struct ViewState {
	/// Last committed URL.
	pub url:   Str,
	/// Last observed document title.
	pub title: Str,
}

/// Shared handle to [`ViewState`].
pub type SharedState = Arc<Mutex<ViewState>>;
