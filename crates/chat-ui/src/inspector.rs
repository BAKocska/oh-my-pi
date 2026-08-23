//! Alternate-screen inspector for canonical pre-rendered history.

use omp_core::sf;
use omp_tui::{Dim, Frame, Key, Layer, Mouse, OverlayOptions, Size, Style};

/// Result of routing history-inspector input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryInspectorEvent {
	/// The inspector remains open after consuming the input.
	Consumed,
	/// Close the inspector and restore the live viewport.
	Close,
}

/// Scrollable view over a canonical, app-rendered history frame.
///
/// Committed terminal rows are immutable; all historical interaction happens
/// on this alternate-screen projection instead of mutating native scrollback.
pub struct HistoryInspector {
	history:  Frame,
	viewport: Frame,
	top:      u16,
	options:  OverlayOptions,
}

impl HistoryInspector {
	/// Opens on the newest rows of an app-rendered canonical history frame.
	pub fn open(history: Frame) -> Self {
		Self {
			history,
			viewport: Frame::new(Size::new(0, 0)),
			top: u16::MAX,
			options: OverlayOptions::default().z(30),
		}
	}

	/// Routes vertical navigation and close keys.
	pub fn handle_key(&mut self, key: Key) -> HistoryInspectorEvent {
		let page = self.viewport.size().height.saturating_sub(2).max(1);
		match key {
			Key::Esc => return HistoryInspectorEvent::Close,
			Key::Up => self.scroll_up(1),
			Key::Down => self.scroll_down(1),
			Key::PageUp => self.scroll_up(page),
			Key::PageDown => self.scroll_down(page),
			Key::Home => self.top = 0,
			Key::End => self.top = self.max_top(page),
			_ => return HistoryInspectorEvent::Consumed,
		}
		self.rebuild();
		HistoryInspectorEvent::Consumed
	}

	/// Routes wheel scrolling; other pointer events remain captured.
	pub fn handle_mouse(&mut self, kind: Mouse) -> HistoryInspectorEvent {
		match kind {
			Mouse::WheelUp => self.scroll_up(3),
			Mouse::WheelDown => self.scroll_down(3),
			_ => return HistoryInspectorEvent::Consumed,
		}
		self.rebuild();
		HistoryInspectorEvent::Consumed
	}

	/// Returns a full-viewport modal layer for alternate-screen presentation.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		if self.viewport.size() != viewport {
			self.viewport = Frame::new(viewport);
			self.options = self.options.width(Dim::Cells(viewport.width));
			self.rebuild();
		}
		Layer { frame: &self.viewport, options: &self.options, active: true }
	}

	fn scroll_up(&mut self, rows: u16) {
		self.top = self.top.saturating_sub(rows);
	}

	fn scroll_down(&mut self, rows: u16) {
		let body_rows = self.viewport.size().height.saturating_sub(2);
		self.top = self.top.saturating_add(rows).min(self.max_top(body_rows));
	}

	fn max_top(&self, body_rows: u16) -> u16 {
		self.history.size().height.saturating_sub(body_rows)
	}

	fn rebuild(&mut self) {
		let size = self.viewport.size();
		self.viewport.clear(Style::default());
		if size.height == 0 {
			return;
		}
		let body_rows = size.height.saturating_sub(2);
		let max_top = self.max_top(body_rows);
		self.top = self.top.min(max_top);
		self
			.viewport
			.put(0, 0, "History inspector", Style::default().bold());
		if body_rows > 0 {
			self.viewport.blit(&self.history, self.top, body_rows, 0, 1);
		}
		if size.height > 1 {
			let first = if self.history.size().height == 0 {
				0
			} else {
				self.top.saturating_add(1)
			};
			let last = self
				.top
				.saturating_add(body_rows)
				.min(self.history.size().height);
			let status = sf!(
				"Rows {first}-{last} of {} · ↑/↓ scroll · PgUp/PgDn page · Esc close",
				self.history.size().height
			);
			self
				.viewport
				.put(0, size.height - 1, status.as_str(), Style::default().dim());
		}
	}
}

#[cfg(test)]
mod tests {
	use omp_tui::{Frame, Key, Size, Style};

	use super::{HistoryInspector, HistoryInspectorEvent};

	#[test]
	fn opens_at_latest_rows_and_closes_on_escape() {
		let mut history = Frame::new(Size::new(20, 12));
		for row in 0..12 {
			history.put(0, row, &format!("row {row}"), Style::default());
		}
		let mut inspector = HistoryInspector::open(history);
		let _ = inspector.layer(Size::new(20, 6));
		assert_eq!(inspector.top, 8);
		assert_eq!(inspector.handle_key(Key::PageUp), HistoryInspectorEvent::Consumed);
		assert_eq!(inspector.top, 4);
		assert_eq!(inspector.handle_key(Key::Esc), HistoryInspectorEvent::Close);
	}
}
