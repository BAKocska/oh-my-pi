//! A live browser pane composited through the wgpu pixel-surface pipeline.
//!
//! The user's installed browser renders headless (`omp-webview` frames
//! surface); every frame is uploaded into a [`PixelSurface`] and drawn as a
//! textured quad, with winit input forwarded back to the page.
//!
//! ```sh
//! cargo run -p omp-gui --example browser -- https://example.com
//! cargo run -p omp-gui --example browser -- --shot https://example.com /tmp/shot.png
//! ```
//!
//! `--shot` runs fully offscreen: first frame -> texture -> quad -> readback
//! -> PNG, no window required.

use std::sync::Arc;

use omp_gui::{Gpu, PixelDraw, PixelPainter, PixelSurface, WindowGpu};
use omp_webview::{
	Engine, FrameConfig, Input, Key, Modifiers, MouseButton, SurfaceKind, WebView, WebViewBuilder,
	WebViewEvent,
};
use winit::{
	application::ApplicationHandler,
	event::{ElementState, MouseScrollDelta, WindowEvent},
	event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
	keyboard::{NamedKey, SmolStr},
	window::{Window, WindowId},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let args: Vec<String> = std::env::args().skip(1).collect();
	if args.first().is_some_and(|a| a == "--shot") {
		let url = args
			.get(1)
			.cloned()
			.unwrap_or_else(|| "https://example.com".into());
		let out = args
			.get(2)
			.cloned()
			.unwrap_or_else(|| "/tmp/browser-shot.png".into());
		return shot(&url, &out);
	}
	if args.first().is_some_and(|a| a == "--delta") {
		return delta_proof();
	}
	let url = args
		.first()
		.cloned()
		.unwrap_or_else(|| "https://example.com".into());
	let event_loop = EventLoop::new()?;
	event_loop.set_control_flow(ControlFlow::Poll);
	let mut app = App { url, pane: None };
	event_loop.run_app(&mut app)?;
	Ok(())
}

/// Everything one live browser pane needs.
struct Pane {
	window:  Arc<Window>,
	gpu:     Gpu,
	target:  WindowGpu,
	painter: PixelPainter,
	surface: PixelSurface,
	view:    WebView,
	cursor:  (f64, f64),
	mods:    Modifiers,
}

struct App {
	url:  String,
	pane: Option<Pane>,
}

impl Pane {
	fn create(event_loop: &ActiveEventLoop, url: &str) -> Result<Self, Box<dyn std::error::Error>> {
		let window = Arc::new(
			event_loop.create_window(
				Window::default_attributes()
					.with_title("omp browser pane")
					.with_inner_size(winit::dpi::LogicalSize::new(1100.0, 750.0)),
			)?,
		);
		let gpu = Gpu::new(None)?;
		let target = WindowGpu::new(&gpu, Arc::clone(&window))?;
		let painter = PixelPainter::new(&gpu, target.format());
		let surface = painter.surface(&gpu);

		let scale = window.scale_factor();
		let logical = window.inner_size().to_logical::<f64>(scale);
		let view = WebViewBuilder::new(Engine::find(SurfaceKind::Frames)?)
			.url(url)
			.build_frames(FrameConfig {
				width: logical.width as u32,
				height: logical.height as u32,
				scale,
				fps_cap: None,
				..FrameConfig::default()
			})?;
		Ok(Self {
			window,
			gpu,
			target,
			painter,
			surface,
			view,
			cursor: (0.0, 0.0),
			mods: Modifiers::NONE,
		})
	}

	/// Drains webview events; returns false once the engine is gone.
	fn pump(&mut self) -> bool {
		// Coalesce to the newest frame; the upload region must union the
		// skipped frames' damage since the texture holds the oldest state.
		let mut latest: Option<(omp_webview::Frame, [u32; 4])> = None;
		for event in self.view.events().try_iter() {
			match event {
				WebViewEvent::Frame(frame) => {
					let damage = match &latest {
						Some((prev, union))
							if (prev.width, prev.height) == (frame.width, frame.height) =>
						{
							union_rect(*union, frame.damage)
						},
						_ => frame.damage,
					};
					latest = Some((frame, damage));
				},
				WebViewEvent::TitleChanged(title) => self.window.set_title(&title),
				WebViewEvent::Closed | WebViewEvent::Crashed(_) => return false,
				_ => {},
			}
		}
		if let Some((frame, damage)) = latest {
			self.surface.upload(
				&self.gpu,
				&self.painter,
				frame.width,
				frame.height,
				&frame.data,
				Some(damage),
			);
			self.window.request_redraw();
		}
		true
	}

	fn redraw(&mut self) {
		let Some(frame) = self.target.acquire(&self.gpu) else {
			return;
		};
		let view = frame
			.texture
			.create_view(&wgpu::TextureViewDescriptor::default());
		let size = self.window.inner_size();
		self.painter.draw(
			&self.gpu,
			&view,
			(size.width, size.height),
			wgpu::LoadOp::Clear(wgpu::Color { r: 0.07, g: 0.07, b: 0.09, a: 1.0 }),
			&[PixelDraw {
				surface: &self.surface,
				dst:     [0.0, 0.0, size.width as f32, size.height as f32],
				opacity: 1.0,
			}],
		);
		self.gpu.queue.present(frame);
	}
}

/// Union of two `[x, y, w, h]` rects.
fn union_rect(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
	let x0 = a[0].min(b[0]);
	let y0 = a[1].min(b[1]);
	let x1 = (a[0] + a[2]).max(b[0] + b[2]);
	let y1 = (a[1] + a[3]).max(b[1] + b[3]);
	[x0, y0, x1 - x0, y1 - y0]
}

/// Maps a winit named key onto the webview key vocabulary.
const fn named_key(key: NamedKey) -> Option<Key> {
	Some(match key {
		NamedKey::Enter => Key::Enter,
		NamedKey::Tab => Key::Tab,
		NamedKey::Backspace => Key::Backspace,
		NamedKey::Delete => Key::Delete,
		NamedKey::Escape => Key::Escape,
		NamedKey::ArrowUp => Key::ArrowUp,
		NamedKey::ArrowDown => Key::ArrowDown,
		NamedKey::ArrowLeft => Key::ArrowLeft,
		NamedKey::ArrowRight => Key::ArrowRight,
		NamedKey::Home => Key::Home,
		NamedKey::End => Key::End,
		NamedKey::PageUp => Key::PageUp,
		NamedKey::PageDown => Key::PageDown,
		_ => return None,
	})
}

impl ApplicationHandler for App {
	fn resumed(&mut self, event_loop: &ActiveEventLoop) {
		match Pane::create(event_loop, &self.url) {
			Ok(pane) => self.pane = Some(pane),
			Err(err) => {
				eprintln!("failed to start browser pane: {err}");
				event_loop.exit();
			},
		}
	}

	fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
		if let Some(pane) = &mut self.pane
			&& !pane.pump()
		{
			event_loop.exit();
		}
	}

	fn window_event(
		&mut self,
		event_loop: &ActiveEventLoop,
		_window_id: WindowId,
		event: WindowEvent,
	) {
		let Some(pane) = &mut self.pane else { return };
		let scale = pane.window.scale_factor();
		match event {
			WindowEvent::CloseRequested => event_loop.exit(),
			WindowEvent::RedrawRequested => pane.redraw(),
			WindowEvent::Resized(size) => {
				pane.target.resize(&pane.gpu, size.width, size.height);
				let logical = size.to_logical::<f64>(scale);
				let _ = pane
					.view
					.resize(logical.width as u32, logical.height as u32);
			},
			WindowEvent::CursorMoved { position, .. } => {
				let p = position.to_logical::<f64>(scale);
				pane.cursor = (p.x, p.y);
				let _ = pane.view.input(Input::MouseMove { x: p.x, y: p.y });
			},
			WindowEvent::MouseInput { state, button, .. } => {
				let button = match button {
					winit::event::MouseButton::Left => MouseButton::Left,
					winit::event::MouseButton::Middle => MouseButton::Middle,
					winit::event::MouseButton::Right => MouseButton::Right,
					_ => return,
				};
				let (x, y) = pane.cursor;
				let _ = pane.view.input(match state {
					ElementState::Pressed => Input::MouseDown { button, x, y, clicks: 1 },
					ElementState::Released => Input::MouseUp { button, x, y },
				});
			},
			WindowEvent::MouseWheel { delta, .. } => {
				let (x, y) = pane.cursor;
				// winit deltas are "content moves" positive-up; browser wheel
				// deltas are positive-down.
				let (dx, dy) = match delta {
					MouseScrollDelta::LineDelta(h, v) => (f64::from(-h) * 40.0, f64::from(-v) * 40.0),
					MouseScrollDelta::PixelDelta(p) => (-p.x, -p.y),
				};
				let _ = pane.view.input(Input::Scroll { x, y, dx, dy });
			},
			WindowEvent::ModifiersChanged(mods) => {
				let state = mods.state();
				pane.mods = Modifiers {
					alt:   state.alt_key(),
					ctrl:  state.control_key(),
					meta:  state.super_key(),
					shift: state.shift_key(),
				};
			},
			WindowEvent::KeyboardInput { event, .. } => {
				let down = event.state == ElementState::Pressed;
				match event.logical_key {
					winit::keyboard::Key::Named(named) => {
						if let Some(key) = named_key(named) {
							let _ = pane.view.input(if down {
								Input::KeyDown { key, modifiers: pane.mods }
							} else {
								Input::KeyUp { key, modifiers: pane.mods }
							});
						}
					},
					winit::keyboard::Key::Character(text) if down => {
						send_text(pane, &text);
					},
					_ => {},
				}
			},
			_ => {},
		}
	}
}

/// Types `text` into the page: plain characters insert as text, chorded
/// characters (ctrl/meta held) go through the key path so shortcuts work.
fn send_text(pane: &Pane, text: &SmolStr) {
	if pane.mods.ctrl || pane.mods.meta {
		if let Some(c) = text.chars().next() {
			let key = Key::Char(c);
			let _ = pane
				.view
				.input(Input::KeyDown { key, modifiers: pane.mods });
			let _ = pane.view.input(Input::KeyUp { key, modifiers: pane.mods });
		}
	} else {
		let _ = pane.view.input(Input::Text(text.as_str().into()));
	}
}

/// Offscreen proof path: first webview frame -> texture -> quad -> readback.
fn shot(url: &str, out: &str) -> Result<(), Box<dyn std::error::Error>> {
	let (width, height) = (800_u32, 600_u32);
	let view = WebViewBuilder::new(Engine::find(SurfaceKind::Frames)?)
		.url(url)
		.build_frames(FrameConfig {
			width,
			height,
			scale: 1.0,
			fps_cap: None,
			// Lossless so the readback proof is pixel-exact.
			format: omp_webview::FrameFormat::Png,
		})?;

	// Wait for the first delivered frame.
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
	let frame = loop {
		match view.events().recv_deadline(deadline)? {
			WebViewEvent::Frame(frame) => break frame,
			WebViewEvent::Crashed(err) => return Err(format!("engine crashed: {err}").into()),
			_ => {},
		}
	};

	let gpu = Gpu::new(None)?;
	let painter = PixelPainter::new(&gpu, wgpu::TextureFormat::Rgba8Unorm);
	let mut surface = painter.surface(&gpu);
	surface.upload(&gpu, &painter, frame.width, frame.height, &frame.data, None);
	let pixels = render_to_pixels(&gpu, &painter, &surface, frame.width, frame.height)?;

	let file = std::fs::File::create(out)?;
	let mut enc = png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
	enc.set_color(png::ColorType::Rgba);
	enc.set_depth(png::BitDepth::Eight);
	enc.write_header()?.write_image_data(&pixels)?;
	println!("offscreen render: {}x{} -> {out}", frame.width, frame.height);
	Ok(())
}

/// Proves the damage-rect delta path: mutate a page region, region-upload
/// every delivered frame, and assert the GPU readback is byte-identical to
/// the engine's final frame.
fn delta_proof() -> Result<(), Box<dyn std::error::Error>> {
	const PAGE: &str = r#"<html><body style="margin:0;background:#dddddd">
		<div id="box" style="position:fixed;left:300px;top:200px;width:60px;height:40px;background:#000"></div>
		</html>"#;
	let (width, height) = (400_u32, 300_u32);
	let view = WebViewBuilder::new(Engine::find(SurfaceKind::Frames)?)
		.html(PAGE)
		.build_frames(FrameConfig {
			width,
			height,
			scale: 1.0,
			fps_cap: None,
			// Lossless so the equality assertion is exact.
			format: omp_webview::FrameFormat::Png,
		})?;

	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
	let first = loop {
		match view.events().recv_deadline(deadline)? {
			WebViewEvent::Frame(frame) => break frame,
			WebViewEvent::Crashed(err) => return Err(format!("engine crashed: {err}").into()),
			_ => {},
		}
	};

	let gpu = Gpu::new(None)?;
	let painter = PixelPainter::new(&gpu, wgpu::TextureFormat::Rgba8Unorm);
	let mut surface = painter.surface(&gpu);
	surface.upload(&gpu, &painter, first.width, first.height, &first.data, Some(first.damage));
	println!("first frame {}x{} damage {:?}", first.width, first.height, first.damage);

	// Mutate a small region, then region-upload every delivered frame.
	view.eval("document.getElementById('box').style.background='#ff0000'")?;
	let mut last = None;
	let quiet = std::time::Duration::from_millis(1500);
	while let Ok(event) = view.events().recv_timeout(quiet) {
		if let WebViewEvent::Frame(frame) = event {
			assert_ne!(
				frame.damage,
				[0, 0, frame.width, frame.height],
				"expected a partial damage rect"
			);
			println!("delta frame damage {:?}", frame.damage);
			surface.upload(&gpu, &painter, frame.width, frame.height, &frame.data, Some(frame.damage));
			last = Some(frame);
		}
	}
	let last = last.ok_or("no frame arrived after the mutation")?;

	let pixels = render_to_pixels(&gpu, &painter, &surface, last.width, last.height)?;
	assert_eq!(
		pixels.as_slice(),
		&last.data[..],
		"region-uploaded texture must equal the engine's final frame"
	);
	println!("delta proof: readback is byte-identical to the final frame");
	Ok(())
}

/// Renders `surface` 1:1 into an offscreen target and reads the pixels back.
fn render_to_pixels(
	gpu: &Gpu,
	painter: &PixelPainter,
	surface: &PixelSurface,
	width: u32,
	height: u32,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
	let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
		label:           Some("readback-target"),
		size:            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
		mip_level_count: 1,
		sample_count:    1,
		dimension:       wgpu::TextureDimension::D2,
		format:          wgpu::TextureFormat::Rgba8Unorm,
		usage:           wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
		view_formats:    &[],
	});
	let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
	painter.draw(
		gpu,
		&target_view,
		(width, height),
		wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
		&[PixelDraw { surface, dst: [0.0, 0.0, width as f32, height as f32], opacity: 1.0 }],
	);

	let row = (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
	let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
		label:              Some("readback"),
		size:               u64::from(row) * u64::from(height),
		usage:              wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
		mapped_at_creation: false,
	});
	let mut encoder = gpu.device.create_command_encoder(&Default::default());
	encoder.copy_texture_to_buffer(
		wgpu::TexelCopyTextureInfo {
			texture:   &target,
			mip_level: 0,
			origin:    wgpu::Origin3d::ZERO,
			aspect:    wgpu::TextureAspect::All,
		},
		wgpu::TexelCopyBufferInfo {
			buffer: &readback,
			layout: wgpu::TexelCopyBufferLayout {
				offset:         0,
				bytes_per_row:  Some(row),
				rows_per_image: Some(height),
			},
		},
		wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
	);
	gpu.queue.submit([encoder.finish()]);
	let slice = readback.slice(..);
	slice.map_async(wgpu::MapMode::Read, |_| {});
	gpu.device.poll(wgpu::PollType::wait_indefinitely())?;

	let data = slice.get_mapped_range()?;
	let mut pixels = Vec::with_capacity((width * height * 4) as usize);
	for y in 0..height {
		let start = (y * row) as usize;
		pixels.extend_from_slice(&data[start..start + (width * 4) as usize]);
	}
	Ok(pixels)
}
