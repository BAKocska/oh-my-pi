//! Stream a page as RGBA frames from the user's installed browser.
//!
//! ```sh
//! cargo run -p omp-webview --example frames -- https://example.com
//! OMP_WEBVIEW_BROWSER=/path/to/firefox cargo run -p omp-webview --example frames
//! ```
//!
//! Saves the first few frames as PNGs under the system temp dir, prints page
//! events for a few seconds, then exits.

use std::time::{Duration, Instant};

use omp_webview::{Engine, FrameConfig, SurfaceKind, WebViewBuilder, WebViewEvent};

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let url = std::env::args()
		.nth(1)
		.unwrap_or_else(|| "https://example.com".into());
	let engine = Engine::find(SurfaceKind::Frames)?;
	println!("engine: {engine:?}");

	let view = WebViewBuilder::new(engine)
		.url(&url)
		.build_frames(FrameConfig {
			width: 800,
			height: 600,
			scale: 1.0,
			fps_cap: Some(10.0),
			..FrameConfig::default()
		})?;

	let deadline = Instant::now() + Duration::from_secs(10);
	let mut saved = 0u32;
	while let Ok(event) = view.events().recv_deadline(deadline) {
		match event {
			WebViewEvent::Frame(frame) => {
				if saved < 3 {
					let path = std::env::temp_dir().join(format!("omp-webview-frame-{saved}.png"));
					let file = std::fs::File::create(&path)?;
					let mut enc =
						png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
					enc.set_color(png::ColorType::Rgba);
					enc.set_depth(png::BitDepth::Eight);
					enc.write_header()?.write_image_data(&frame.data)?;
					println!("frame {}x{} -> {}", frame.width, frame.height, path.display());
					saved += 1;
				}
			},
			WebViewEvent::Closed | WebViewEvent::Crashed(_) => {
				println!("event: {event:?}");
				break;
			},
			other => println!("event: {other:?}"),
		}
	}
	println!("final url={} title={:?}", view.url(), view.title());
	Ok(())
}
