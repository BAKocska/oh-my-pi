//! Sanitized provider-stream viewer with tail-follow and drop accounting.

use omp_core::{Str, sf};
use omp_tui::{Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, dom};

use crate::{OverlayPanel, panel_divider};

/// One inference-owned, already-redacted stream frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawFrame {
	/// Monotonic ring sequence.
	pub sequence: u64,
	/// Session binding, when known.
	pub session:  Option<Str>,
	/// Provider event name or frame category.
	pub event:    Str,
	/// Sanitized frame payload.
	pub payload:  Str,
}

/// Ring summary projected by inference.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StreamSummary {
	/// Frames currently retained.
	pub retained:         usize,
	/// Frames evicted from the bounded ring.
	pub evicted:          u64,
	/// Subscriber deliveries dropped due to backpressure.
	pub subscriber_drops: u64,
}

/// Raw-stream viewer interaction result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawStreamEvent {
	/// Viewer remains open.
	Consumed,
	/// Close the overlay.
	Close,
	/// Copy this sanitized visible frame.
	Copy(String),
}

/// Pretty SSE/JSON viewer over inference-owned snapshots and subscriptions.
pub struct RawStreamViewer {
	frames:  Vec<RawFrame>,
	summary: StreamSummary,
	cursor:  usize,
	follow:  bool,
	pretty:  bool,
	ui:      Ui,
	ctx:     UiContext,
	options: OverlayOptions,
	width:   u16,
	rows:    u16,
}

impl RawStreamViewer {
	/// Opens a viewer on a bounded ring snapshot.
	pub fn open(frames: Vec<RawFrame>, summary: StreamSummary, ctx: &UiContext) -> Self {
		let cursor = frames.len().saturating_sub(1);
		let mut viewer = Self {
			frames,
			summary,
			cursor,
			follow: true,
			pretty: true,
			ui: Ui::from_root(dom! { <text/> }, 1, ctx.clone()),
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Cells(88))
				.z(20),
			width: 88,
			rows: 18,
		};
		viewer.rebuild();
		viewer
	}

	/// Appends one subscribed frame and preserves explicit navigation away from
	/// tail.
	pub fn push(&mut self, frame: RawFrame, summary: StreamSummary) {
		self.frames.push(frame);
		self.summary = summary;
		if self.follow {
			self.cursor = self.frames.len().saturating_sub(1);
		}
		self.rebuild();
	}

	/// Replaces the viewer from a fresh bounded snapshot after ring eviction.
	pub fn replace(&mut self, frames: Vec<RawFrame>, summary: StreamSummary) {
		self.frames = frames;
		self.summary = summary;
		self.cursor = if self.follow {
			self.frames.len().saturating_sub(1)
		} else {
			self.cursor.min(self.frames.len().saturating_sub(1))
		};
		self.rebuild();
	}

	/// Routes navigation, tail-follow, pretty-print, and clipboard keys.
	pub fn handle_key(&mut self, key: Key) -> RawStreamEvent {
		match key {
			Key::Esc => return RawStreamEvent::Close,
			Key::Up => self.move_cursor(-1),
			Key::Down => self.move_cursor(1),
			Key::PageUp => self.move_cursor(-(self.rows as isize)),
			Key::PageDown => self.move_cursor(self.rows as isize),
			Key::Home => {
				self.cursor = 0;
				self.follow = false;
			},
			Key::End => {
				self.cursor = self.frames.len().saturating_sub(1);
				self.follow = true;
			},
			Key::Char('f') => self.follow = !self.follow,
			Key::Char('p') => self.pretty = !self.pretty,
			Key::Copy | Key::Ctrl('c') => return RawStreamEvent::Copy(self.current_text()),
			_ => return RawStreamEvent::Consumed,
		}
		self.rebuild();
		RawStreamEvent::Consumed
	}

	/// Routes wheel navigation; any upward navigation disables tail-follow.
	pub fn handle_mouse(&mut self, kind: Mouse) -> RawStreamEvent {
		match kind {
			Mouse::WheelUp => self.move_cursor(-3),
			Mouse::WheelDown => self.move_cursor(3),
			_ => return RawStreamEvent::Consumed,
		}
		self.rebuild();
		RawStreamEvent::Consumed
	}

	/// Returns the responsive overlay layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = viewport.width.saturating_sub(4).max(36);
		let rows = viewport.height.saturating_sub(8).max(6);
		if width != self.width || rows != self.rows {
			self.width = width;
			self.rows = rows;
			self.rebuild();
		}
		self.options = self.options.width(Dim::Cells(width));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn move_cursor(&mut self, delta: isize) {
		if self.frames.is_empty() {
			return;
		}
		self.cursor = self
			.cursor
			.saturating_add_signed(delta)
			.min(self.frames.len() - 1);
		self.follow = self.cursor + 1 == self.frames.len() && delta >= 0;
	}

	fn current_text(&self) -> String {
		let Some(frame) = self.frames.get(self.cursor) else {
			return String::new();
		};
		if !self.pretty {
			return frame.payload.to_string();
		}
		pretty_payload(&frame.payload)
	}

	fn rebuild(&mut self) {
		let current = self.frames.get(self.cursor);
		let title = current
			.map_or_else(|| sf!("No frames"), |frame| sf!("#{} · {}", frame.sequence, frame.event));
		let payload = self.current_text();
		let summary = sf!(
			"{} retained · {} evicted · {} subscriber drops · {}",
			self.summary.retained,
			self.summary.evicted,
			self.summary.subscriber_drops,
			if self.follow {
				"following tail"
			} else {
				"paused"
			}
		);
		let height = self.rows;
		self.ui = Ui::from_root(
			OverlayPanel::new("Raw provider stream").child(dom! {
				<col gap=1>
					<text bold truncate>{title}</text>
					<text dim truncate>{summary}</text>
					<scroll id="raw-stream-scroll" h={height}><text wrap>{payload}</text></scroll>
					{panel_divider()}
					<text dim truncate>{"↑/↓ navigate · F follow · P pretty · Ctrl+C copy · Esc close"}</text>
				</col>
			}),
			self.width,
			self.ctx.clone(),
		);
	}
}

fn pretty_payload(payload: &str) -> String {
	if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
		return serde_json::to_string_pretty(&value).unwrap_or_else(|_| payload.to_owned());
	}
	let mut output = String::with_capacity(payload.len());
	for line in payload.lines() {
		if let Some(data) = line.strip_prefix("data:") {
			output.push_str("data:");
			let data = data.trim();
			if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
				output.push('\n');
				output
					.push_str(&serde_json::to_string_pretty(&value).unwrap_or_else(|_| data.to_owned()));
			} else {
				output.push(' ');
				output.push_str(data);
			}
		} else {
			output.push_str(line);
		}
		output.push('\n');
	}
	output
}
