//! `WKWebView` child surface: a native subview of the host window.
//!
//! An in-process `WKWebView` added as a subview of the host window's content
//! view — wry's model, ported onto this crate's contract. Main-thread-only:
//! `AppKit` requires it, and [`WkView`] is `!Send` because it holds
//! `Retained` Objective-C objects.

use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSView};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::ns_string;
use objc2_web_kit::{WKUserContentController, WKWebView};
use omp_core::Str;
use raw_window_handle::RawWindowHandle;

use super::{
	ConfiguredPage, IpcHandler, NavDelegate, TitleObserver, check_main, configure_page, eval,
	initial_load, install_observers, load_html, navigate, style_webview,
};
use crate::{
	error::{Error, Result},
	event::{SharedState, WebViewEvent},
	geometry::Rect,
	options::PageOptions,
};

/// Converts contract bounds (top-left origin, y down) into an `AppKit` frame
/// for a subview of `parent` (bottom-left origin unless the view is flipped).
fn appkit_frame(parent: &NSView, bounds: Rect) -> CGRect {
	let origin = if parent.isFlipped() {
		CGPoint::new(bounds.x, bounds.y)
	} else {
		let parent_height = parent.frame().size.height;
		CGPoint::new(bounds.x, parent_height - bounds.y - bounds.height)
	};
	CGRect { origin, size: CGSize::new(bounds.width, bounds.height) }
}

/// A `WKWebView` embedded as a child of a host window's content view.
///
/// `!Send` by construction (holds `Retained` `AppKit` objects); every method
/// additionally verifies the main thread and fails with [`Error::MainThread`].
pub struct WkView {
	/// The embedded webview.
	webview: Retained<WKWebView>,
	/// The configuration's user-content controller (scripts + IPC handler).
	manager: Retained<WKUserContentController>,
	/// Keeps the IPC handler alive; unregistered by name in `Drop`.
	_ipc:    Retained<IpcHandler>,
	/// Keeps the navigation delegate alive (`WebKit` only holds a weak ref).
	_nav:    Retained<NavDelegate>,
	/// Keeps the title KVO observer alive; unregisters itself on drop.
	_title:  Retained<TitleObserver>,
}

impl WkView {
	/// Builds the webview per `page`, wires delegates/observers to `events` and
	/// `state`, inserts it into `parent` at `bounds`, and starts the initial
	/// load.
	pub(crate) fn create(
		page: &PageOptions,
		parent: RawWindowHandle,
		bounds: Rect,
		events: flume::Sender<WebViewEvent>,
		state: SharedState,
	) -> Result<Self> {
		let mtm = MainThreadMarker::new().ok_or(Error::MainThread)?;
		let RawWindowHandle::AppKit(handle) = parent else {
			return Err(Error::WindowHandle);
		};
		// SAFETY: an AppKit handle's ns_view is a live NSView pointer for the
		// lifetime of the host window, and we are on the main thread.
		let ns_view: &NSView = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };

		let ConfiguredPage { config, manager, ipc } = configure_page(page, events.clone(), mtm);

		let frame = appkit_frame(ns_view, bounds);
		// SAFETY: designated WKWebView initializer with a finite frame and the
		// fully configured `config`, on the main thread.
		let webview = unsafe {
			WKWebView::initWithFrame_configuration(mtm.alloc::<WKWebView>(), frame, &config)
		};
		// Positioned explicitly via `set_bounds`; never auto-resized.
		webview.setAutoresizingMask(NSAutoresizingMaskOptions::ViewNotSizable);
		style_webview(&webview, page);
		let (nav, title) = install_observers(&webview, events, state, None, mtm);

		ns_view.addSubview(&webview);

		let view = Self { webview, manager, _ipc: ipc, _nav: nav, _title: title };
		initial_load(&view.webview, page)?;
		Ok(view)
	}

	/// Navigate to `url`.
	pub(crate) fn navigate(&self, url: &str) -> Result<()> {
		check_main()?;
		navigate(&self.webview, url)
	}

	/// Replace the document with `html` (null origin).
	pub(crate) fn load_html(&self, html: &str) -> Result<()> {
		check_main()?;
		load_html(&self.webview, html);
		Ok(())
	}

	/// Evaluate `js`; when `reply` is set it receives the JSON-encoded result
	/// (string results verbatim, everything else via `NSJSONSerialization` with
	/// fragments allowed) on the main thread.
	pub(crate) fn eval(&self, js: &str, reply: Option<Box<dyn FnOnce(Str) + Send>>) -> Result<()> {
		check_main()?;
		eval(&self.webview, js, reply);
		Ok(())
	}

	/// Reload the current page.
	pub(crate) fn reload(&self) -> Result<()> {
		check_main()?;
		// SAFETY: reloads the current page; navigation token unused.
		let _ = unsafe { self.webview.reload() };
		Ok(())
	}

	/// History back.
	pub(crate) fn back(&self) -> Result<()> {
		check_main()?;
		// SAFETY: no-op when there is no back item; navigation token unused.
		let _ = unsafe { self.webview.goBack() };
		Ok(())
	}

	/// History forward.
	pub(crate) fn forward(&self) -> Result<()> {
		check_main()?;
		// SAFETY: no-op when there is no forward item; navigation token unused.
		let _ = unsafe { self.webview.goForward() };
		Ok(())
	}

	/// Make the webview the host window's first responder.
	pub(crate) fn focus(&self) -> Result<()> {
		check_main()?;
		if let Some(window) = self.webview.window() {
			let _ = window.makeFirstResponder(Some(&self.webview));
		}
		Ok(())
	}

	/// Reposition within the parent view, converting from top-left-origin
	/// logical points into `AppKit` coordinates.
	pub(crate) fn set_bounds(&self, bounds: Rect) -> Result<()> {
		check_main()?;
		// SAFETY: reading the superview link of our own live webview.
		if let Some(parent) = unsafe { self.webview.superview() } {
			self.webview.setFrame(appkit_frame(&parent, bounds));
		}
		Ok(())
	}

	/// Show or hide the webview.
	pub(crate) fn set_visible(&self, visible: bool) -> Result<()> {
		check_main()?;
		self.webview.setHidden(!visible);
		Ok(())
	}
}

impl Drop for WkView {
	fn drop(&mut self) {
		// `WkView` is !Send, so Drop runs on the main thread. Break the
		// controller → handler retain edge, then detach from the window; the
		// title observer unregisters its KVO in its own Drop.
		// SAFETY: the handler was registered under "ipc" in `create`, and the
		// webview detaches from a live (or absent) superview.
		unsafe {
			self
				.manager
				.removeScriptMessageHandlerForName(ns_string!("ipc"));
			self.webview.removeFromSuperview();
		}
	}
}
