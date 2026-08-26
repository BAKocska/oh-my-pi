use omp_core::{IntoStr, Str};

use super::text::{append, paint_rich, truncate_rich};
use crate::{
	UiContext,
	component::{Cached, Component, IntoChildren, MemoKey, PaintCtx, Slot, next_slot},
	frame::Rect,
	markdown,
	markdown::MdTheme,
	markup,
	props::{Prop, PropValue, Props},
	rich::{Measure, RichText, cell_width},
};

/// Rendered Markdown content backing the `<markdown>` markup tag.
pub struct Markdown {
	props:          Props,
	slot:           Slot,
	text:           Str,
	source:         Str,
	rich:           RichText,
	embedded:       Vec<Cached>,
	version:        u64,
	cached_width:   u16,
	cached_partial: bool,
	cached:         Option<MemoKey>,
	fast_tail:      Option<markdown::FastTail>,
	measured:       Option<(MemoKey, (u16, u16), u16)>,
}

impl Markdown {
	/// Creates an empty Markdown block.
	pub fn new() -> Self {
		Self {
			props:          Props::new(),
			slot:           next_slot(),
			text:           Str::default(),
			source:         Str::default(),
			rich:           RichText::default(),
			embedded:       Vec::new(),
			version:        1,
			cached_width:   0,
			cached_partial: false,
			cached:         None,
			fast_tail:      None,
			measured:       None,
		}
	}

	/// Creates a Markdown block containing the supplied source.
	pub fn text_of(text: impl IntoStr) -> Self {
		Self::new().text(text)
	}

	/// Sets one Markdown property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self.version = self.version.wrapping_add(1);
		self
	}

	/// Sets one Markdown property from a string.
	pub fn with_str(self, prop: Prop, value: &str) -> Self {
		self.with(prop, value)
	}

	/// Appends Markdown source text.
	pub fn text(mut self, text: impl IntoStr) -> Self {
		let text = text.into_str();
		append(&mut self.source, text.clone());
		append(&mut self.text, text);
		self.version = self.version.wrapping_add(1);
		self
	}

	/// Appends embedded components referenced by the Markdown source.
	pub fn child(mut self, child: impl IntoChildren) -> Self {
		child.extend_children(&mut self.embedded);
		self
	}

	fn theme(&self, ctx: &UiContext) -> MdTheme {
		MdTheme::from_context(ctx).cascade(self.props.style(&ctx.theme))
	}

	fn render(&mut self, ctx: &UiContext, width: u16) {
		let width = width.max(1);
		let key = MemoKey::new(self.version, ctx);
		let partial = self.props.partial();
		if self.cached_width == width && self.cached_partial == partial && self.cached == Some(key) {
			return;
		}
		let theme = self.theme(ctx);
		let style = self.props.style(&ctx.theme);
		if partial
			&& self.props.truncate().is_none()
			&& self
				.fast_tail
				.as_mut()
				.is_some_and(|tail| tail.splice(&self.text, width, &theme, &mut self.rich))
		{
			self.cached_width = width;
			self.cached_partial = true;
			self.cached = Some(key);
			return;
		}
		self.rich.clear();
		if partial {
			self.fast_tail =
				markdown::render_partial_capturing(&self.text, width, &theme, &mut self.rich);
		} else {
			markdown::render(&self.text, width, &theme, &mut self.rich);
			self.fast_tail = None;
		}
		truncate_rich(&mut self.rich, width, style, self.props.truncate());
		if self.props.truncate().is_some() {
			self.fast_tail = None;
		}
		self.cached_width = width;
		self.cached_partial = partial;
		self.cached = Some(key);
	}
}

impl Default for Markdown {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for Markdown {
	fn props(&self) -> &Props {
		&self.props
	}

	fn props_mut(&mut self) -> &mut Props {
		&mut self.props
	}

	fn slot(&self) -> Slot {
		self.slot
	}

	fn children(&self) -> &[Cached] {
		&self.embedded
	}

	fn children_mut(&mut self) -> &mut [Cached] {
		&mut self.embedded
	}

	fn measure(&mut self, ctx: &UiContext) -> (u16, u16) {
		let key = MemoKey::new(self.version, ctx);
		if self.props.partial()
			&& self.embedded.is_empty()
			&& let Some((cached, measured, _)) = self.measured
			&& cached == key
		{
			return measured;
		}
		let theme = self.theme(ctx);
		let mut natural = Measure::default();
		if self.props.partial() {
			markdown::render_partial(&self.text, u16::MAX, &theme, &mut natural);
		} else {
			markdown::render(&self.text, u16::MAX, &theme, &mut natural);
		}
		let mut min = natural.widest.clamp(1, 12);
		let mut nat = natural.widest.max(min);
		for child in &mut self.embedded {
			if child.visible {
				let (child_min, child_nat) = child.measure(ctx);
				min = min.max(child_min);
				nat = nat.max(child_nat);
			}
		}
		let measured = (min, nat);
		self.measured = (self.props.partial() && self.embedded.is_empty()).then_some((
			key,
			measured,
			natural.final_width(),
		));
		measured
	}

	fn height(&mut self, ctx: &UiContext, width: u16) -> u16 {
		self.render(ctx, width);
		let mut height = if self.text.is_empty() {
			0
		} else {
			RichText::rows(&self.rich)
		};
		let mut placed = !self.text.is_empty();
		for child in &mut self.embedded {
			if !child.visible {
				continue;
			}
			if placed {
				height = height.saturating_add(1);
			}
			height = height.saturating_add(child.height(ctx, width));
			placed = true;
		}
		height
	}

	fn place(&mut self, ctx: &UiContext, content: Rect) {
		self.render(ctx, content.width);
		let mut cursor = content.y;
		let mut placed = if self.text.is_empty() {
			false
		} else {
			cursor = cursor.saturating_add(RichText::rows(&self.rich));
			true
		};
		for child in &mut self.embedded {
			if !child.visible {
				continue;
			}
			if placed {
				cursor = cursor.saturating_add(1);
			}
			let height = child.height(ctx, content.width);
			child.place(ctx, Rect::new(content.x, cursor, content.width, height));
			cursor = cursor.saturating_add(height);
			placed = true;
		}
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if !self.text.is_empty() {
			self.render(pc.ctx, rect.width);
			let own = Rect::new(rect.x, rect.y, rect.width, RichText::rows(&self.rich));
			paint_rich(pc, own, &self.rich, self.props.align());
		}
		for child in &mut self.embedded {
			if child.visible {
				child.paint(pc);
			}
		}
	}

	fn set_text(&mut self, ctx: &UiContext, text: Str) -> bool {
		if self.source == text {
			return false;
		}
		let embeds_markup = markup::md_embeds_markup(&text);
		let old_key = MemoKey::new(self.version, ctx);
		let delta_width = text
			.as_str()
			.strip_prefix(self.source.as_str())
			.filter(|delta| !delta.contains('\t'))
			.filter(|delta| {
				delta
					.chars()
					.last()
					.is_none_or(|character| !character.is_whitespace())
			})
			.map(cell_width);
		let theme = self.theme(ctx);
		let fast = !embeds_markup
			&& self.embedded.is_empty()
			&& self.props.partial()
			&& self.props.truncate().is_none()
			&& self.cached == Some(old_key)
			&& self.fast_tail.as_mut().is_some_and(|tail| {
				tail.splice(&text, self.cached_width.max(1), &theme, &mut self.rich)
			});
		self.source = text.clone();
		if embeds_markup {
			if let Ok(children) = markup::parse_md_fragment_inheriting(&text, ctx, &self.props) {
				self.text = Str::default();
				self.embedded = children;
			} else {
				self.text = text;
				self.embedded.clear();
			}
		} else {
			self.text = text;
			self.embedded.clear();
		}
		self.version = self.version.wrapping_add(1);
		if fast {
			let key = MemoKey::new(self.version, ctx);
			self.cached = Some(key);
			if let (Some(delta_width), Some((measured_key, measured, tail_width))) =
				(delta_width, self.measured)
				&& measured_key == old_key
			{
				let tail_width = tail_width.saturating_add(delta_width);
				let widest = measured.1.max(tail_width);
				let min = widest.clamp(1, 12);
				self.measured = Some((key, (min, widest.max(min)), tail_width));
			} else {
				self.measured = None;
			}
		} else {
			self.fast_tail = None;
			self.measured = None;
		}
		true
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::context::UiContext;

	#[test]
	fn set_text_regrafts_static_markup_in_document_order() {
		let ctx = UiContext::default();
		let mut markdown = Markdown::text_of("old");
		assert!(markdown.set_text(&ctx, Str::new("before\n<box><text>inside</text></box>\nafter"),));
		assert!(markdown.text.is_empty());
		assert_eq!(markdown.embedded.len(), 3);
	}

	#[test]
	fn set_text_degrades_rejected_dynamic_markup_to_literal_text() {
		let ctx = UiContext::default();
		for source in ["<input/>", "<box id=duplicate/>", "<box when=\"x == y\"/>", "</md>"] {
			let mut markdown = Markdown::text_of("old");
			assert!(markdown.set_text(&ctx, Str::new(source)));
			assert_eq!(markdown.text, source);
			assert!(markdown.embedded.is_empty());
		}
	}

	#[test]
	fn retained_partial_text_splices_plain_streaming_delta() {
		let ctx = UiContext::default();
		let mut markdown = Markdown::new()
			.with(Prop::Partial, true)
			.text("A plain paragraph tail");
		let _ = markdown.measure(&ctx);
		markdown.render(&ctx, 12);
		assert!(markdown.set_text(&ctx, Str::new("A plain paragraph tail grows")));
		assert_eq!(
			markdown
				.fast_tail
				.as_ref()
				.expect("plain paragraph remains captured")
				.splice_count(),
			1,
		);
		let theme = markdown.theme(&ctx);
		let mut cold = RichText::default();
		markdown::render_partial(&markdown.text, 12, &theme, &mut cold);
		assert_eq!(markdown.rich.rows(), cold.rows());
		for row in 0..cold.rows() {
			assert_eq!(markdown.rich.row_text(row), cold.row_text(row));
			assert_eq!(
				markdown.rich.row_runs(row).collect::<Vec<_>>(),
				cold.row_runs(row).collect::<Vec<_>>(),
			);
		}
	}
}
