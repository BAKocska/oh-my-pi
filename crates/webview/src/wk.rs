//! System-WKWebView child-surface backend (macOS).
//!
//! An in-process `WKWebView` added as a subview of the host window's content
//! view — wry's model, ported onto this crate's contract. The whole module is
//! main-thread-only: `AppKit` requires it, and [`WkView`] is `!Send` because it
//! holds `Retained` Objective-C objects.

use objc2::{
	AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
	rc::Retained,
	runtime::{AnyObject, NSObject, ProtocolObject},
	sel,
};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSColor, NSView};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{
	NSDictionary, NSJSONSerialization, NSJSONWritingOptions, NSKeyValueChangeKey,
	NSKeyValueObservingOptions, NSNumber, NSObjectNSKeyValueCoding,
	NSObjectNSKeyValueObserverRegistration, NSObjectProtocol, NSString, NSURL, NSURLRequest,
	NSUTF8StringEncoding, ns_string,
};
use objc2_web_kit::{
	WKNavigation, WKNavigationDelegate, WKScriptMessage, WKScriptMessageHandler,
	WKUserContentController, WKUserScript, WKUserScriptInjectionTime, WKWebView,
	WKWebViewConfiguration, WKWebsiteDataStore,
};
use omp_core::{Str, fmts};
use parking_lot::Mutex;
use raw_window_handle::RawWindowHandle;

use crate::{
	error::{Error, Result},
	event::{SharedState, WebViewEvent},
	geometry::Rect,
	options::PageOptions,
};

/// JS shim that routes `window.ipc.postMessage` onto the `WebKit`
/// script-message handler registered as `ipc`, fulfilling the crate-wide IPC
/// contract.
const IPC_SHIM: &str = "Object.defineProperty(window, 'ipc', { value: Object.freeze({ \
                        postMessage: function(s) { \
                        window.webkit.messageHandlers.ipc.postMessage(s) } }) });";

/// Last committed URL of `webview` as a `Str`, empty when nothing is loaded.
fn current_url(webview: &WKWebView) -> Str {
	// SAFETY: `WKWebView::URL` only reads the webview's current navigation state.
	unsafe { webview.URL() }
		.and_then(|url| url.absoluteString())
		.map(|s| fmts!("{s}"))
		.unwrap_or_default()
}

/// Current document title of `webview` as a `Str`, empty when absent.
fn current_title(webview: &WKWebView) -> Str {
	// SAFETY: `WKWebView::title` only reads the webview's current document title.
	unsafe { webview.title() }
		.map(|s| fmts!("{s}"))
		.unwrap_or_default()
}

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

/// Ivars of [`IpcHandler`]: the channel IPC payloads are forwarded onto.
struct IpcHandlerIvars {
	/// View event channel; send failures mean the host hung up and are ignored.
	events: flume::Sender<WebViewEvent>,
}

define_class!(
	/// `WKScriptMessageHandler` receiving `window.ipc.postMessage` payloads and
	/// forwarding them as [`WebViewEvent::Ipc`].
	#[unsafe(super(NSObject))]
	#[thread_kind = MainThreadOnly]
	#[ivars = IpcHandlerIvars]
	struct IpcHandler;

	unsafe impl NSObjectProtocol for IpcHandler {}

	unsafe impl WKScriptMessageHandler for IpcHandler {
		/// Entry point for messages posted to `webkit.messageHandlers.ipc`.
		#[unsafe(method(userContentController:didReceiveScriptMessage:))]
		fn did_receive(&self, _controller: &WKUserContentController, msg: &WKScriptMessage) {
			// Only string bodies participate in the IPC contract; other types
			// (numbers, objects) are silently dropped like in wry.
			// SAFETY: `msg` is a live WKScriptMessage delivered by WebKit on
			// the main thread; `body` returns a retained plist object.
			let body = unsafe { msg.body() };
			if let Ok(body) = body.downcast::<NSString>() {
				let _ = self.ivars().events.send(WebViewEvent::Ipc(fmts!("{body}")));
			}
		}
	}
);

impl IpcHandler {
	/// Allocates the handler and registers it on `controller` under `ipc`.
	fn new(
		controller: &WKUserContentController,
		events: flume::Sender<WebViewEvent>,
		mtm: MainThreadMarker,
	) -> Retained<Self> {
		let this = mtm.alloc::<Self>().set_ivars(IpcHandlerIvars { events });
		// SAFETY: plain NSObject `init` on a freshly allocated instance.
		let this: Retained<Self> = unsafe { msg_send![super(this), init] };
		// SAFETY: `this` conforms to WKScriptMessageHandler; the controller
		// retains it and `WkView::drop` removes it again by name.
		unsafe {
			controller
				.addScriptMessageHandler_name(ProtocolObject::from_ref(&*this), ns_string!("ipc"));
		}
		this
	}
}

/// Ivars of [`NavDelegate`]: event channel plus the shared url/title cache.
struct NavDelegateIvars {
	/// View event channel; send failures mean the host hung up and are ignored.
	events: flume::Sender<WebViewEvent>,
	/// Shared url/title state kept current as navigations progress.
	state:  SharedState,
}

define_class!(
	/// `WKNavigationDelegate` translating WebKit navigation callbacks into
	/// [`WebViewEvent`]s and keeping [`SharedState`] current.
	#[unsafe(super(NSObject))]
	#[thread_kind = MainThreadOnly]
	#[ivars = NavDelegateIvars]
	struct NavDelegate;

	unsafe impl NSObjectProtocol for NavDelegate {}

	unsafe impl WKNavigationDelegate for NavDelegate {
		/// A page began loading.
		#[unsafe(method(webView:didStartProvisionalNavigation:))]
		fn did_start(&self, webview: &WKWebView, _navigation: &WKNavigation) {
			let _ = self
				.ivars()
				.events
				.send(WebViewEvent::LoadStarted(current_url(webview)));
		}

		/// A navigation committed: the new document is now current.
		#[unsafe(method(webView:didCommitNavigation:))]
		fn did_commit(&self, webview: &WKWebView, _navigation: &WKNavigation) {
			let url = current_url(webview);
			self.ivars().state.lock().url = url.clone();
			let _ = self.ivars().events.send(WebViewEvent::Navigated(url));
		}

		/// The page finished loading; also refresh the cached title, since the
		/// title KVO may fire before the final document title settles.
		#[unsafe(method(webView:didFinishNavigation:))]
		fn did_finish(&self, webview: &WKWebView, _navigation: &WKNavigation) {
			self.ivars().state.lock().title = current_title(webview);
			let _ = self
				.ivars()
				.events
				.send(WebViewEvent::LoadFinished(current_url(webview)));
		}
	}
);

impl NavDelegate {
	/// Allocates the delegate; the caller installs it via
	/// `setNavigationDelegate`.
	fn new(
		events: flume::Sender<WebViewEvent>,
		state: SharedState,
		mtm: MainThreadMarker,
	) -> Retained<Self> {
		let this = mtm
			.alloc::<Self>()
			.set_ivars(NavDelegateIvars { events, state });
		// SAFETY: plain NSObject `init` on a freshly allocated instance.
		unsafe { msg_send![super(this), init] }
	}
}

/// Ivars of [`TitleObserver`]: the observed webview (retained so the KVO
/// registration can always be undone) plus event channel and state cache.
struct TitleObserverIvars {
	/// The observed webview; retained until the observer unregisters in `Drop`.
	webview: Retained<WKWebView>,
	/// View event channel; send failures mean the host hung up and are ignored.
	events:  flume::Sender<WebViewEvent>,
	/// Shared url/title state kept current as the title changes.
	state:   SharedState,
}

define_class!(
	/// KVO observer on the webview's `title` key path (wry's
	/// `DocumentTitleChangedObserver`), emitting [`WebViewEvent::TitleChanged`].
	#[unsafe(super(NSObject))]
	#[ivars = TitleObserverIvars]
	struct TitleObserver;

	unsafe impl NSObjectProtocol for TitleObserver {}

	/// NSKeyValueObserving callback.
	impl TitleObserver {
		/// Fires whenever the observed `title` key path changes.
		#[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
		fn observe_value(
			&self,
			key_path: Option<&NSString>,
			of_object: Option<&AnyObject>,
			_change: Option<&NSDictionary<NSKeyValueChangeKey, AnyObject>>,
			_context: *mut std::ffi::c_void,
		) {
			let observed = key_path.is_some_and(|k| k.isEqualToString(ns_string!("title")));
			if observed && of_object.is_some() {
				let title = current_title(&self.ivars().webview);
				self.ivars().state.lock().title = title.clone();
				let _ = self.ivars().events.send(WebViewEvent::TitleChanged(title));
			}
		}
	}
);

impl TitleObserver {
	/// Allocates the observer and registers it for `title` changes on `webview`.
	fn new(
		webview: Retained<WKWebView>,
		events: flume::Sender<WebViewEvent>,
		state: SharedState,
	) -> Retained<Self> {
		let this = Self::alloc().set_ivars(TitleObserverIvars { webview, events, state });
		// SAFETY: plain NSObject `init` on a freshly allocated instance.
		let this: Retained<Self> = unsafe { msg_send![super(this), init] };
		// SAFETY: KVO on the retained webview's `title` key with a null
		// context; the registration is undone in `Drop` before either side
		// deallocates.
		unsafe {
			this.ivars().webview.addObserver_forKeyPath_options_context(
				&this,
				ns_string!("title"),
				NSKeyValueObservingOptions::New,
				std::ptr::null_mut(),
			);
		}
		this
	}
}

impl Drop for TitleObserver {
	fn drop(&mut self) {
		// Unregister before the retained webview goes away; a live KVO
		// registration on a deallocating object aborts the process.
		// SAFETY: removes the registration made in `new`; `self` and the
		// retained webview are both still alive here.
		unsafe {
			self
				.ivars()
				.webview
				.removeObserver_forKeyPath(self, ns_string!("title"));
		};
	}
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

		// SAFETY: creating a fresh configuration on the main thread.
		let config = unsafe { WKWebViewConfiguration::new(mtm) };
		if page.incognito {
			// SAFETY: swapping the fresh config's data store for an in-memory
			// one before the webview is created.
			unsafe {
				config.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm));
			}
		}

		// Suppress the opaque white page background before the first paint;
		// `drawsBackground` is the same private KVC key wry relies on.
		if page.transparent || page.background.is_some() {
			let no = NSNumber::numberWithBool(false);
			// SAFETY: `drawsBackground` is a boolean KVC key WKWebViewConfiguration
			// understands (private but stable; wry ships the same call).
			unsafe { config.setValue_forKey(Some(&no), ns_string!("drawsBackground")) };
		}

		// IPC shim first, then user init scripts in order — all at document
		// start, all frames — so `window.ipc` exists before any of them run.
		// SAFETY: reading the fresh config's controller on the main thread.
		let manager = unsafe { config.userContentController() };
		let ipc = IpcHandler::new(&manager, events.clone(), mtm);
		for script in std::iter::once(IPC_SHIM).chain(page.init_scripts.iter().map(Str::as_str)) {
			// SAFETY: designated WKUserScript initializer with a valid source
			// string on the main thread.
			let user_script = unsafe {
				WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
					WKUserScript::alloc(mtm),
					&NSString::from_str(script),
					WKUserScriptInjectionTime::AtDocumentStart,
					false,
				)
			};
			// SAFETY: appending the freshly created script to the controller.
			unsafe { manager.addUserScript(&user_script) };
		}

		let frame = appkit_frame(ns_view, bounds);
		// SAFETY: designated WKWebView initializer with a finite frame and the
		// fully configured `config`, on the main thread.
		let webview = unsafe {
			WKWebView::initWithFrame_configuration(mtm.alloc::<WKWebView>(), frame, &config)
		};
		// Positioned explicitly via `set_bounds`; never auto-resized.
		webview.setAutoresizingMask(NSAutoresizingMaskOptions::ViewNotSizable);

		if page.transparent {
			// Runtime half of the transparency dance: the instance-level
			// `drawsBackground` KVC key plus a clear overscroll color.
			// SAFETY: same private-but-stable `drawsBackground` KVC key as on
			// the config; `setOpaque:`/`setUnderPageBackgroundColor:` are only
			// sent after respondsToSelector confirms the receiver handles them.
			unsafe {
				let no = NSNumber::numberWithBool(false);
				webview.setValue_forKey(Some(&no), ns_string!("drawsBackground"));
				if webview.respondsToSelector(sel!(setOpaque:)) {
					let () = msg_send![&webview, setOpaque: false];
				}
				if webview.respondsToSelector(sel!(setUnderPageBackgroundColor:)) {
					webview.setUnderPageBackgroundColor(Some(&NSColor::clearColor()));
				}
			}
		} else if let Some([r, g, b, a]) = page.background {
			// Solid background: paint the overscroll/under-page area since
			// `drawsBackground` is already disabled on the config.
			let color = NSColor::colorWithSRGBRed_green_blue_alpha(
				f64::from(r) / 255.0,
				f64::from(g) / 255.0,
				f64::from(b) / 255.0,
				f64::from(a) / 255.0,
			);
			if webview.respondsToSelector(sel!(setUnderPageBackgroundColor:)) {
				// SAFETY: selector presence checked on the line above.
				unsafe { webview.setUnderPageBackgroundColor(Some(&color)) };
			}
		}

		if let Some(user_agent) = &page.user_agent {
			// SAFETY: overriding the UA string with a valid NSString.
			unsafe { webview.setCustomUserAgent(Some(&NSString::from_str(user_agent))) };
		}

		if page.devtools {
			// `isInspectable` (macOS 13.3+) gates Safari Web Inspector access;
			// `developerExtrasEnabled` is the private key wry also flips for
			// the in-view inspector on older systems.
			if webview.respondsToSelector(sel!(setInspectable:)) {
				// SAFETY: selector presence checked on the line above (macOS 13.3+).
				unsafe { webview.setInspectable(true) };
			}
			let yes = NSNumber::numberWithBool(true);
			// SAFETY: `developerExtrasEnabled` is a boolean KVC key WKPreferences
			// understands (private but stable; wry ships the same call).
			unsafe {
				config
					.preferences()
					.setValue_forKey(Some(&yes), ns_string!("developerExtrasEnabled"));
			}
		}

		let nav = NavDelegate::new(events.clone(), state.clone(), mtm);
		// SAFETY: `nav` conforms to WKNavigationDelegate; WebKit holds it
		// weakly and `WkView` keeps it retained for the webview's lifetime.
		unsafe { webview.setNavigationDelegate(Some(ProtocolObject::from_ref(&*nav))) };
		let title = TitleObserver::new(webview.clone(), events, state);

		ns_view.addSubview(&webview);

		let view = Self { webview, manager, _ipc: ipc, _nav: nav, _title: title };
		match (&page.url, &page.html) {
			(Some(url), _) => view.navigate(url)?,
			(None, Some(html)) => view.load_html(html)?,
			(None, None) => view.navigate("about:blank")?,
		}
		Ok(view)
	}

	/// Fails with [`Error::MainThread`] unless called on the main thread.
	///
	/// Belt-and-suspenders: the type is already `!Send`, so reaching this off
	/// the main thread requires unsafe caller code.
	fn check_main() -> Result<MainThreadMarker> {
		debug_assert!(MainThreadMarker::new().is_some(), "WkView used off the main thread");
		MainThreadMarker::new().ok_or(Error::MainThread)
	}

	/// Navigate to `url`.
	pub(crate) fn navigate(&self, url: &str) -> Result<()> {
		Self::check_main()?;
		let ns_url = NSURL::URLWithString(&NSString::from_str(url))
			.ok_or_else(|| Error::Protocol(fmts!("invalid url: {url}")))?;
		let request = NSURLRequest::requestWithURL(&ns_url);
		// SAFETY: starting a load with a valid request; returned navigation
		// token is unused.
		let _ = unsafe { self.webview.loadRequest(&request) };
		Ok(())
	}

	/// Replace the document with `html` (null origin).
	pub(crate) fn load_html(&self, html: &str) -> Result<()> {
		Self::check_main()?;
		// SAFETY: loading a valid HTML string with a nil base URL (null origin).
		let _ = unsafe {
			self
				.webview
				.loadHTMLString_baseURL(&NSString::from_str(html), None)
		};
		Ok(())
	}

	/// Evaluate `js`; when `reply` is set it receives the JSON-encoded result
	/// (string results verbatim, everything else via `NSJSONSerialization` with
	/// fragments allowed) on the main thread.
	pub(crate) fn eval(&self, js: &str, reply: Option<Box<dyn FnOnce(Str) + Send>>) -> Result<()> {
		Self::check_main()?;
		let js = NSString::from_str(js);
		match reply {
			// SAFETY: evaluating a valid script with no completion handler.
			None => unsafe { self.webview.evaluateJavaScript_completionHandler(&js, None) },
			Some(reply) => {
				// The completion block is `Fn`, our callback `FnOnce`: park it
				// in a Mutex<Option<..>> and take it on the single invocation.
				let reply = Mutex::new(Some(reply));
				let handler = block2::RcBlock::new(
					move |val: *mut AnyObject, _err: *mut objc2_foundation::NSError| {
						let Some(reply) = reply.lock().take() else {
							return;
						};
						// SAFETY: WebKit passes null or a valid result object
						// to the completion block, exactly the contract of
						// `json_of_eval_result`.
						reply(unsafe { json_of_eval_result(val) });
					},
				);
				// SAFETY: evaluating a valid script; the RcBlock is copied by
				// WebKit and outlives this scope.
				unsafe {
					self
						.webview
						.evaluateJavaScript_completionHandler(&js, Some(&handler));
				};
			},
		}
		Ok(())
	}

	/// Reload the current page.
	pub(crate) fn reload(&self) -> Result<()> {
		Self::check_main()?;
		// SAFETY: reloads the current page; navigation token unused.
		let _ = unsafe { self.webview.reload() };
		Ok(())
	}

	/// History back.
	pub(crate) fn back(&self) -> Result<()> {
		Self::check_main()?;
		// SAFETY: no-op when there is no back item; navigation token unused.
		let _ = unsafe { self.webview.goBack() };
		Ok(())
	}

	/// History forward.
	pub(crate) fn forward(&self) -> Result<()> {
		Self::check_main()?;
		// SAFETY: no-op when there is no forward item; navigation token unused.
		let _ = unsafe { self.webview.goForward() };
		Ok(())
	}

	/// Make the webview the host window's first responder.
	pub(crate) fn focus(&self) -> Result<()> {
		Self::check_main()?;
		if let Some(window) = self.webview.window() {
			let _ = window.makeFirstResponder(Some(&self.webview));
		}
		Ok(())
	}

	/// Reposition within the parent view, converting from top-left-origin
	/// logical points into `AppKit` coordinates.
	pub(crate) fn set_bounds(&self, bounds: Rect) -> Result<()> {
		Self::check_main()?;
		// SAFETY: reading the superview link of our own live webview.
		if let Some(parent) = unsafe { self.webview.superview() } {
			self.webview.setFrame(appkit_frame(&parent, bounds));
		}
		Ok(())
	}

	/// Show or hide the webview.
	pub(crate) fn set_visible(&self, visible: bool) -> Result<()> {
		Self::check_main()?;
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

/// Converts an `evaluateJavaScript` completion value into JSON text matching
/// the remote engines' `eval` replies: `nil` as `null`, `NSString` results
/// JSON-encoded (quoted), anything else serialized with `NSJSONSerialization`
/// allowing fragments (numbers, booleans, null).
///
/// # Safety
///
/// `val` must be null or a valid Objective-C object pointer, as delivered by
/// `WebKit` to the completion handler.
unsafe fn json_of_eval_result(val: *mut AnyObject) -> Str {
	if val.is_null() {
		return Str::new("null");
	}
	// SAFETY: non-null per the check above; validity per this fn's contract.
	let val = unsafe { &*val };
	if let Some(s) = val.downcast_ref::<NSString>() {
		// JSON-encode so string results arrive quoted, like CDP/BiDi replies.
		return serde_json::to_string(&*s.to_string())
			.map(Str::new)
			.unwrap_or_default();
	}
	// SAFETY: `val` is a valid object; non-plist objects yield Err, not UB.
	let Ok(data) = (unsafe {
		NSJSONSerialization::dataWithJSONObject_options_error(
			val,
			NSJSONWritingOptions::FragmentsAllowed,
		)
	}) else {
		return Str::default();
	};
	NSString::initWithData_encoding(NSString::alloc(), &data, NSUTF8StringEncoding)
		.map(|s| fmts!("{s}"))
		.unwrap_or_default()
}
