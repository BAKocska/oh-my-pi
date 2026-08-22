//! Attachment image browser backed by the composer's retained attachments.

use omp_core::{Str, sf};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext,
	components::AttachmentContent, dom,
};

use crate::{
	Attachment,
	overlays::{OverlayPanel, panel_divider},
};

/// Image overlay input outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageOverlayEvent {
	/// Input was consumed and the browser remains open.
	Consumed,
	/// Close the browser without modifying attachments.
	Close,
}

#[derive(Clone)]
struct ImageRow {
	source:     Str,
	dimensions: Option<(u32, u32)>,
	marker:     usize,
}

/// Retained attachment browser with bounded zoom and metadata.
pub struct ImageOverlay {
	ui:      Ui,
	images:  Vec<ImageRow>,
	current: usize,
	zoom:    u16,
	ctx:     UiContext,
	options: OverlayOptions,
	width:   u16,
}

impl ImageOverlay {
	/// Opens over image attachments only; text attachments remain staged but
	/// hidden.
	pub fn open(attachments: &[Attachment], ctx: &UiContext) -> Self {
		let images = attachments
			.iter()
			.filter_map(|attachment| match &attachment.content {
				AttachmentContent::Image { source, dimensions } => Some(ImageRow {
					source:     source.clone(),
					dimensions: *dimensions,
					marker:     attachment.marker,
				}),
				AttachmentContent::Text { .. } => None,
			})
			.collect::<Vec<_>>();
		let width = 80;
		let zoom = 24;
		let ui = build(&images, 0, zoom, width, ctx);
		Self {
			ui,
			images,
			current: 0,
			zoom,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Pct(90))
				.z(10),
			width,
		}
	}

	/// Routes selection and zoom without mutating the staged attachment queue.
	pub fn handle_key(&mut self, key: Key) -> ImageOverlayEvent {
		match key {
			Key::Esc => return ImageOverlayEvent::Close,
			Key::Left if !self.images.is_empty() => {
				self.current = self.current.checked_sub(1).unwrap_or(self.images.len() - 1);
			},
			Key::Right if !self.images.is_empty() => {
				self.current = (self.current + 1) % self.images.len();
			},
			Key::Char('+') | Key::Char('=') => self.zoom = self.zoom.saturating_add(8).min(72),
			Key::Char('-') => self.zoom = self.zoom.saturating_sub(8).max(8),
			_ => return ImageOverlayEvent::Consumed,
		}
		self.rebuild();
		ImageOverlayEvent::Consumed
	}

	/// Clicking outside closes; wheel input changes zoom.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> ImageOverlayEvent {
		if self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
			.is_none()
		{
			if kind == Mouse::Click {
				return ImageOverlayEvent::Close;
			}
		}
		match kind {
			Mouse::WheelUp => self.zoom = self.zoom.saturating_add(8).min(72),
			Mouse::WheelDown => self.zoom = self.zoom.saturating_sub(8).max(8),
			_ => return ImageOverlayEvent::Consumed,
		}
		self.rebuild();
		ImageOverlayEvent::Consumed
	}

	/// Returns the centered responsive layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		if self.width != viewport.width {
			self.width = viewport.width;
			self.rebuild();
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn rebuild(&mut self) {
		self.ui = build(&self.images, self.current, self.zoom, self.width, &self.ctx);
	}
}

fn build(images: &[ImageRow], current: usize, zoom: u16, width: u16, ctx: &UiContext) -> Ui {
	let image = images.get(current);
	let source = image.map(|image| image.source.clone()).unwrap_or_default();
	let label = image.map_or_else(
		|| sf!("No staged image attachments."),
		|image| {
			image.dimensions.map_or_else(
				|| sf!("image #{} · dimensions unavailable · zoom {}", image.marker, zoom),
				|(w, h)| sf!("image #{} · {}x{} · zoom {}", image.marker, w, h, zoom),
			)
		},
	);
	let count = if images.is_empty() {
		sf!("0 / 0")
	} else {
		sf!("{} / {}", current + 1, images.len())
	};
	Ui::from_root(
		OverlayPanel::new("Images").child(dom! {
			<col align=center>
				if !source.is_empty() { <img src={source} w={zoom}/> }
				{panel_divider()}
				<text fg=fg>{label}</text>
				<text fg=muted>{count}{" · ←/→ select · +/- zoom · Esc close"}</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}
