//! Round-trip IPC, JS evaluation, and synthetic input on a frames surface.
//!
//! ```sh
//! cargo run -p omp-webview --example ipc
//! OMP_WEBVIEW_BROWSER=/path/to/firefox cargo run -p omp-webview --example ipc
//! ```
//!
//! Exits 0 once init-script IPC, page IPC, `eval_with`, and a synthetic click
//! have all been observed; exits 1 on timeout.

use std::time::{Duration, Instant};

use omp_webview::{
	Engine, FrameConfig, Input, MouseButton, SurfaceKind, WebViewBuilder, WebViewEvent,
};

const PAGE: &str = r#"<html><body>
<button id="b" style="position:fixed;left:0;top:0;width:200px;height:100px">click me</button>
<script>
document.getElementById('b').addEventListener('click', () => window.ipc.postMessage('clicked'));
window.addEventListener('load', () => window.ipc.postMessage('loaded'));
</script></body></html>"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let engine = Engine::find(SurfaceKind::Frames)?;
	println!("engine: {engine:?}");

	let view = WebViewBuilder::new(engine)
		.html(PAGE)
		.init_script("window.ipc.postMessage('init-ran')")
		.build_frames(FrameConfig { width: 400, height: 300, ..FrameConfig::default() })?;

	let (mut init_ran, mut loaded, mut evaled, mut clicked) = (false, false, false, false);
	let (eval_tx, eval_rx) = flume::bounded::<String>(1);
	let deadline = Instant::now() + Duration::from_secs(20);

	while !(init_ran && loaded && evaled && clicked) && Instant::now() < deadline {
		if loaded
			&& !evaled
			&& let Ok(result) = eval_rx.try_recv()
		{
			println!("eval 1+2 -> {result}");
			evaled = result == "3";
			// Page is interactive: click the button synthetically.
			view.input(Input::MouseMove { x: 100.0, y: 50.0 })?;
			view.input(Input::MouseDown {
				button: MouseButton::Left,
				x:      100.0,
				y:      50.0,
				clicks: 1,
			})?;
			view.input(Input::MouseUp { button: MouseButton::Left, x: 100.0, y: 50.0 })?;
		}
		let Ok(event) = view.events().recv_timeout(Duration::from_millis(250)) else {
			continue;
		};
		match event {
			WebViewEvent::Ipc(msg) => {
				println!("ipc: {msg}");
				match msg.as_str() {
					"init-ran" => init_ran = true,
					"loaded" => {
						loaded = true;
						let tx = eval_tx.clone();
						view.eval_with("1+2", move |result| {
							let _ = tx.send(result.as_str().to_owned());
						})?;
					},
					"clicked" => clicked = true,
					_ => {},
				}
			},
			WebViewEvent::Frame(_) => {},
			other => println!("event: {other:?}"),
		}
	}

	println!("init={init_ran} loaded={loaded} eval={evaled} clicked={clicked}");
	let ok = init_ran && loaded && evaled && clicked;
	// Drop the view before exiting: `process::exit` skips destructors, and the
	// engine teardown lives in the WebView drop.
	drop(view);
	std::process::exit(i32::from(!ok));
}
