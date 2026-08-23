//! Open a page in an engine-owned OS window (`chrome --app`-style).
//!
//! ```sh
//! cargo run -p omp-webview --example window -- https://example.com        # until closed
//! cargo run -p omp-webview --example window -- https://example.com 5     # auto-close after 5s
//! ```

use std::{
	env, error,
	time::{Duration, Instant},
};

use omp_webview::{Engine, SurfaceKind, WebViewBuilder, WebViewEvent, WindowConfig};

fn main() -> Result<(), Box<dyn error::Error>> {
	let url = env::args()
		.nth(1)
		.unwrap_or_else(|| "https://example.com".into());
	let secs: Option<u64> = env::args().nth(2).and_then(|s| s.parse().ok());
	let engine = Engine::find(SurfaceKind::Window)?;
	println!("engine: {engine:?}");

	let view = WebViewBuilder::new(engine)
		.url(&url)
		.build_window(WindowConfig { width: 900, height: 700 })?;

	let deadline = secs.map(|s| Instant::now() + Duration::from_secs(s));
	loop {
		match view.events().recv_timeout(Duration::from_millis(250)) {
			Ok(event @ (WebViewEvent::Closed | WebViewEvent::Crashed(_))) => {
				println!("event: {event:?}");
				break;
			},
			Ok(event) => println!("event: {event:?}"),
			Err(flume::RecvTimeoutError::Timeout) => {},
			Err(flume::RecvTimeoutError::Disconnected) => break,
		}
		if deadline.is_some_and(|d| Instant::now() >= d) {
			println!("auto-closing");
			break;
		}
	}
	println!("final url={} title={:?}", view.url(), view.title());
	Ok(())
}
