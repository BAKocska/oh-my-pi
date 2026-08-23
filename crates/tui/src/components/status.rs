use omp_core::{IntoStr, Str};
use smallvec::SmallVec;

use super::hr::truncate_to_width;
use crate::{
	Icon, Style,
	component::{Component, PaintCtx, Slot, next_slot},
	context::{Charset, Theme, UiContext},
	frame::{Color, Rect},
	markup::Align,
	props::{Prop, PropValue, Props},
	rich::cell_width,
};

/// Placement of a composer's primary status line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusPlacement {
	/// The status occupies composer chrome, such as a box's top border.
	Embedded,
	/// The status occupies its own row outside the editable surface.
	Standalone,
}

/// Presentation of context-window usage in a status line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextGaugeMode {
	/// Show context usage as a numeric segment.
	Numeric,
	/// Use the flexible boundary between status groups as a proportional bar.
	Bar,
}

/// Horizontal slots for two status groups separated by a flexible boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundaryLayout {
	/// First column of the left group.
	pub left_x:         u16,
	/// First column of the flexible boundary.
	pub boundary_x:     u16,
	/// Width of the flexible boundary.
	pub boundary_width: u16,
	/// First column of the right group.
	pub right_x:        u16,
}

/// Fits left and right status groups around a flexible boundary.
///
/// Returns `None` when both groups plus `minimum_boundary` do not fit.
pub const fn boundary_layout(
	x: u16,
	width: u16,
	left_width: u16,
	right_width: u16,
	minimum_boundary: u16,
) -> Option<BoundaryLayout> {
	let occupied = left_width.saturating_add(right_width);
	if occupied.saturating_add(minimum_boundary) > width {
		return None;
	}
	let boundary_width = width - occupied;
	let boundary_x = x.saturating_add(left_width);
	Some(BoundaryLayout {
		left_x: x,
		boundary_x,
		boundary_width,
		right_x: boundary_x.saturating_add(boundary_width),
	})
}

/// Number of cells filled by a proportional context gauge.
pub const fn context_gauge_cells(width: u16, used: u64, total: u64) -> u16 {
	if total == 0 {
		return 0;
	}
	let used = if used > total { total } else { used };
	((width as u128 * used as u128 + total as u128 / 2) / total as u128) as u16
}

/// Returns the themed accent shared by the compaction threshold marker and
/// context-window usage labels.
pub const fn compaction_threshold_color(theme: &Theme) -> Color {
	theme.accent
}

/// Formats primary-model spend for metered or subscription billing.
///
/// Subscription-backed spend uses the dedicated Nerd Font icon where
/// available and an `S` prefix elsewhere. A zero-cost subscription still
/// renders its semantic subscription marker.
pub fn spend_label(amount_nanos: u64, subscription: bool, charset: Charset) -> Str {
	if amount_nanos == 0 {
		return if subscription {
			Str::new(charset.icon(Icon::Subscription))
		} else {
			Str::default()
		};
	}
	let amount = amount_nanos as f64 / 1_000_000_000.0;
	if !subscription {
		return Str::from(format!("${amount:.4}"));
	}
	match charset {
		Charset::NerdFont => Str::from(format!("{} {amount:.4}", charset.icon(Icon::Subscription))),
		Charset::Unicode | Charset::Ascii => Str::from(format!("S{amount:.4}")),
	}
}

/// Formats advisor-model spend with the charset's advisor degradation.
pub fn advisor_spend_label(amount_nanos: u64, subscription: bool, charset: Charset) -> Str {
	let spend = spend_label(amount_nanos, subscription, charset);
	if spend.is_empty() {
		return spend;
	}
	match charset {
		Charset::Ascii => Str::from(format!("{spend} {}", charset.icon(Icon::Advisor))),
		Charset::Unicode | Charset::NerdFont => {
			Str::from(format!("{} {spend}", charset.icon(Icon::Advisor)))
		},
	}
}

/// Declarative segment data backing the `<segment>` markup tag.
pub struct Segment {
	props: Props,
	label: Str,
}

impl Segment {
	/// Creates an empty status segment.
	pub fn new() -> Self {
		Self { props: Props::new(), label: Str::default() }
	}

	/// Appends label text.
	pub fn label(mut self, label: impl IntoStr) -> Self {
		let label = label.into_str();
		if self.label.is_empty() {
			self.label = label;
		} else {
			self.label = Str::from(format!("{}{}", self.label, label));
		}
		self
	}

	/// Sets one segment property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self
	}

	/// Sets one custom segment property.
	pub fn with_custom(mut self, name: impl IntoStr, value: impl Into<PropValue>) -> Self {
		self.props.set_custom(name, value);
		self
	}
}

impl Default for Segment {
	fn default() -> Self {
		Self::new()
	}
}

/// A one-line powerline-style status group backing the `<status>` markup tag.
///
/// `align=end` (`right`) mirrors the caps for a band docked against the right
/// edge: the opening cap points into the background and the closing edge sits
/// solid on the margin.
pub struct Status {
	props:       Props,
	slot:        Slot,
	segments:    SmallVec<Segment, 8>,
	text_widths: SmallVec<u16, 8>,
}

impl Status {
	/// Creates an empty status group.
	pub fn new() -> Self {
		Self {
			props:       Props::new(),
			slot:        next_slot(),
			segments:    SmallVec::new(),
			text_widths: SmallVec::new(),
		}
	}

	/// Sets one status property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self
	}

	/// Sets one status property from a string.
	pub fn with_str(mut self, prop: Prop, value: &str) -> Self {
		self.props.set(prop, value);
		self
	}

	/// Appends a segment to the group.
	pub fn segment(mut self, segment: Segment) -> Self {
		self.push_segment(segment);
		self
	}

	/// Replaces the segments while preserving this component's slot identity.
	pub fn set_segments(&mut self, segments: impl IntoIterator<Item = Segment>) {
		self.segments.clear();
		self.text_widths.clear();
		for segment in segments {
			self.push_segment(segment);
		}
	}

	fn push_segment(&mut self, segment: Segment) {
		let width = self
			.text_widths
			.last()
			.copied()
			.unwrap_or(0)
			.saturating_add(cell_width(&segment.label));
		self.segments.push(segment);
		self.text_widths.push(width);
	}

	/// Band chrome for this group's dock side.
	fn chrome(&self, charset: Charset) -> (&'static str, &'static str, &'static str) {
		match self.props.align() {
			Align::End => charset.status_band_end(),
			Align::Start | Align::Center => charset.status_band(),
		}
	}

	fn group_width(&self, count: usize, charset: Charset) -> u16 {
		let (left_cap, separator, cap) = self.chrome(charset);
		let text = count
			.checked_sub(1)
			.and_then(|index| self.text_widths.get(index))
			.copied()
			.unwrap_or(0);
		let separators = u16::try_from(count.saturating_sub(1))
			.unwrap_or(u16::MAX)
			.saturating_mul(cell_width(separator).saturating_add(2));
		text
			.saturating_add(separators)
			.saturating_add(cell_width(left_cap))
			.saturating_add(2)
			.saturating_add(cell_width(cap))
	}
}

impl Default for Status {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for Status {
	fn props(&self) -> &Props {
		&self.props
	}

	fn props_mut(&mut self) -> &mut Props {
		&mut self.props
	}

	fn slot(&self) -> Slot {
		self.slot
	}

	fn measure(&mut self, ctx: &UiContext) -> (u16, u16) {
		let min = self.group_width(self.segments.len().min(1), ctx.charset);
		let natural = self.group_width(self.segments.len(), ctx.charset);
		(min, natural)
	}

	fn height(&mut self, _ctx: &UiContext, _width: u16) -> u16 {
		1
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if rect.y >= pc.clip || rect.width == 0 {
			return;
		}
		let mut visible = self.segments.len();
		while visible > 1 && self.group_width(visible, pc.ctx.charset) > rect.width {
			visible -= 1;
		}
		let style = self.props.style(&pc.ctx.theme);
		let (left_cap, separator, cap) = self.chrome(pc.ctx.charset);
		let edge_style = Style::new().fg(style.background_color());
		let left_width = cell_width(left_cap);
		let cap_width = cell_width(cap);
		let truncate_first = visible == 1 && self.group_width(visible, pc.ctx.charset) > rect.width;
		let boundary_width = left_width.saturating_add(cap_width);
		if truncate_first && boundary_width <= rect.width {
			let interior = rect.width - boundary_width;
			let left_pad = interior >= 2;
			let right_pad = interior >= 3;
			let fit = interior
				.saturating_sub(u16::from(left_pad))
				.saturating_sub(u16::from(right_pad));
			let segment = &self.segments[0];
			let mut segment_style = segment.props.style(&pc.ctx.theme).inherit(style);
			if segment_style.background_color() == Color::Default {
				segment_style = segment_style.bg(style.background_color());
			}
			let label = truncate_to_width(&segment.label, fit);
			let mut column = pc.frame.put(rect.x, rect.y, left_cap, edge_style);
			if left_pad {
				column = pc.frame.put(column, rect.y, " ", style);
			}
			column = pc.frame.put(column, rect.y, label.text, segment_style);
			if label.ellipsis {
				column = pc.frame.put(column, rect.y, "…", segment_style);
			}
			if right_pad {
				column = pc.frame.put(column, rect.y, " ", style);
			}
			pc.frame.put(column, rect.y, cap, edge_style);
			return;
		}
		let chrome_width = boundary_width.saturating_add(2);
		if rect.width < chrome_width {
			if left_width <= rect.width {
				pc.frame.put(rect.x, rect.y, left_cap, edge_style);
			}
			if left_width.saturating_add(cap_width) <= rect.width {
				pc.frame
					.put(rect.x.saturating_add(rect.width - cap_width), rect.y, cap, edge_style);
			}
			return;
		}
		let mut column = pc.frame.put(rect.x, rect.y, left_cap, edge_style);
		column = pc.frame.put(column, rect.y, " ", style);
		for (index, segment) in self.segments[..visible].iter().enumerate() {
			if index > 0 {
				column = pc.frame.put(column, rect.y, " ", style.dim());
				column = pc.frame.put(column, rect.y, separator, style.dim());
				column = pc.frame.put(column, rect.y, " ", style.dim());
			}
			let mut segment_style = segment.props.style(&pc.ctx.theme).inherit(style);
			if segment_style.background_color() == Color::Default {
				segment_style = segment_style.bg(style.background_color());
			}
			column = pc.frame.put(column, rect.y, &segment.label, segment_style);
		}
		column = pc.frame.put(column, rect.y, " ", style);
		pc.frame.put(column, rect.y, cap, edge_style);
	}

	fn paints_background(&self) -> bool {
		false
	}
}

#[cfg(test)]
mod tests {
	use super::{
		Segment, Status, advisor_spend_label, boundary_layout, context_gauge_cells, spend_label,
	};
	use crate::{
		Charset, Color, Prop, Ui, UiContext,
		component::{Cached, Hit, PaintCtx},
		dom,
		frame::{Frame, Rect, Size},
		test_support::frame_row_text,
	};

	fn paint(status: Status, width: u16) -> (Frame, Vec<Hit>) {
		paint_with_charset(status, width, Charset::default())
	}

	fn paint_with_charset(status: Status, width: u16, charset: Charset) -> (Frame, Vec<Hit>) {
		let ctx = UiContext { charset, ..UiContext::default() };
		let mut status = Cached::new(Box::new(status));
		status.place(&ctx, Rect::new(0, 0, width, 1));
		let mut frame = Frame::new(Size::new(width, 1));
		let mut hits = Vec::new();
		status.paint(&mut PaintCtx::new(&mut frame, &ctx, &mut hits, &mut Vec::new()));
		(frame, hits)
	}

	#[test]
	fn status_paints_segments_and_styles() {
		let status = Status::new()
			.with(Prop::Bg, "yellow")
			.segment(Segment::new().label("alpha").with(Prop::Fg, "red"))
			.segment(
				Segment::new()
					.label("beta")
					.with(Prop::Fg, "green")
					.with(Prop::Bg, "blue"),
			)
			.segment(Segment::new().label("gamma").with(Prop::Fg, "blue"));
		let (frame, hits) = paint(status, 40);
		assert_eq!(frame_row_text(&frame, 0), " alpha › beta › gamma ›");
		assert_eq!(frame.cell(1, 0).style.foreground_color(), Color::Rgb(255, 0, 0));
		assert_eq!(frame.cell(9, 0).style.foreground_color(), Color::Rgb(0, 128, 0));
		assert_eq!(frame.cell(16, 0).style.foreground_color(), Color::Rgb(0, 0, 255));
		assert_eq!(frame.cell(1, 0).style.background_color(), Color::Rgb(255, 255, 0),);
		assert_eq!(frame.cell(9, 0).style.background_color(), Color::Rgb(0, 0, 255));
		assert_eq!(
			frame.cell(22, 0).style.foreground_color(),
			Color::Rgb(255, 255, 0),
			"the cap uses the band's background as its foreground",
		);
		assert_eq!(
			frame.cell(22, 0).style.background_color(),
			Color::Default,
			"the cap transitions onto the surrounding background",
		);
		assert_eq!(
			frame.cell(23, 0).style.background_color(),
			Color::Default,
			"the band stops after the rendered group",
		);
		assert!(hits.is_empty());
	}

	#[test]
	fn nerd_font_edges_use_band_background_as_foreground() {
		let status = Status::new()
			.with(Prop::Bg, "yellow")
			.segment(Segment::new().label("chip"));
		let (frame, _) = paint_with_charset(status, 20, Charset::NerdFont);

		assert_eq!(frame_row_text(&frame, 0), "\u{e0b6} chip \u{e0b0}");
		for column in [0, 7] {
			assert_eq!(frame.cell(column, 0).style.foreground_color(), Color::Rgb(255, 255, 0),);
			assert_eq!(frame.cell(column, 0).style.background_color(), Color::Default);
		}
		assert_eq!(frame.cell(8, 0).style.background_color(), Color::Default);
	}

	#[test]
	fn align_end_mirrors_the_caps_for_a_right_docked_band() {
		let status = Status::new()
			.with_str(Prop::Align, "right")
			.with(Prop::Bg, "yellow")
			.segment(Segment::new().label("chip"));
		let (frame, _) = paint_with_charset(status, 20, Charset::NerdFont);
		assert_eq!(frame_row_text(&frame, 0), "\u{e0b2} chip");
		assert_eq!(
			frame.cell(6, 0).style.background_color(),
			Color::Rgb(255, 255, 0),
			"the flat closing edge keeps the band background through its pad cell",
		);
		let (frame, _) = paint(
			Status::new()
				.with_str(Prop::Align, "right")
				.segment(Segment::new().label("alpha"))
				.segment(Segment::new().label("beta")),
			20,
		);
		assert_eq!(frame_row_text(&frame, 0), "‹ alpha › beta");
	}

	#[test]
	fn status_narrow_width_drops_whole_trailing_segments() {
		let status = Status::new()
			.segment(Segment::new().label("alpha"))
			.segment(Segment::new().label("beta"))
			.segment(Segment::new().label("gamma"));
		let (frame, _) = paint(status, 10);
		let painted = frame_row_text(&frame, 0);
		assert_eq!(painted, " alpha ›");
		assert!(!painted.contains("beta"));
	}

	#[test]
	fn status_truncates_its_last_chip_at_boundary_widths() {
		for (width, expected) in [(7, " alp… ›"), (4, " … ›"), (3, " …›"), (2, "…›")]
		{
			let status = Status::new().segment(Segment::new().label("alphabet"));
			let (frame, _) = paint(status, width);
			assert_eq!(frame_row_text(&frame, 0), expected);
		}
	}

	#[test]
	fn status_markup_paints_segment_labels() {
		let ui = Ui::from_markup(
			"<status><segment fg=green>alpha</segment><segment>beta</segment></status>",
			40,
			UiContext::default(),
		)
		.expect("status markup should parse");
		let painted = frame_row_text(ui.frame(), 0);
		assert!(painted.contains("alpha › beta"));
	}

	#[test]
	fn status_markup_rejects_orphan_segment() {
		let error = Ui::from_markup("<segment>alpha</segment>", 40, UiContext::default())
			.err()
			.expect("orphan segment must fail");
		assert!(
			error
				.message
				.contains("<segment> is not allowed directly inside")
		);
	}

	#[test]
	fn status_macro_paints_segment_label() {
		let ui = Ui::from_root(
			dom! { <status><segment fg=green>{"alpha"}</segment></status> },
			40,
			UiContext::default(),
		);
		assert!(frame_row_text(ui.frame(), 0).contains("alpha"));
	}
	#[test]
	fn boundary_layout_docks_groups_and_reserves_the_gap() {
		let layout = boundary_layout(3, 30, 8, 6, 2).expect("groups fit");
		assert_eq!(layout.left_x, 3);
		assert_eq!(layout.boundary_x, 11);
		assert_eq!(layout.boundary_width, 16);
		assert_eq!(layout.right_x, 27);
		assert_eq!(boundary_layout(0, 12, 6, 5, 2), None);
	}

	#[test]
	fn boundary_layout_runs_to_the_edge_when_the_right_group_is_empty() {
		let layout = boundary_layout(1, 38, 10, 0, 2).expect("left group and gauge fit");
		assert_eq!(layout.boundary_x, 11);
		assert_eq!(layout.boundary_width, 28);
		assert_eq!(layout.right_x, 39);
	}

	#[test]
	fn context_gauge_rounds_and_clamps_to_its_boundary() {
		assert_eq!(context_gauge_cells(20, 25, 100), 5);
		assert_eq!(context_gauge_cells(9, 50, 100), 5);
		assert_eq!(context_gauge_cells(20, 200, 100), 20);
		assert_eq!(context_gauge_cells(20, 1, 0), 0);
	}

	#[test]
	fn billing_labels_degrade_by_charset() {
		assert_eq!(spend_label(250_000_000, false, Charset::Ascii), "$0.2500");
		assert_eq!(spend_label(250_000_000, true, Charset::Ascii), "S0.2500");
		assert_eq!(spend_label(0, true, Charset::Unicode), "(sub)");
		assert_eq!(spend_label(250_000_000, true, Charset::NerdFont), "\u{f067a} 0.2500",);
	}

	#[test]
	fn advisor_billing_uses_semantic_glyphs() {
		assert_eq!(advisor_spend_label(250_000_000, false, Charset::Ascii), "$0.2500 (adv)",);
		assert_eq!(advisor_spend_label(250_000_000, true, Charset::Unicode), "👁 S0.2500",);
		assert_eq!(
			advisor_spend_label(250_000_000, true, Charset::NerdFont),
			"\u{ea70} \u{f067a} 0.2500",
		);
	}
}
