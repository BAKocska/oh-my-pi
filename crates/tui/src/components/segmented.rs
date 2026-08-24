use omp_core::Str;

use super::{button::soft_colors, select::SelectOption};
use crate::{
	component::{Component, EventCtx, Flow, Hit, HitTag, PaintCtx, Slot, next_slot},
	context::UiContext,
	frame::{Rect, Style},
	input::{Key, Mouse},
	props::{Prop, PropValue, Props},
	rich::cell_width,
};

struct OptionData {
	value: Str,
	icon:  Str,
	label: Str,
}

#[derive(Default)]
struct SegmentedState {
	options: Vec<OptionData>,
	idx:     u16,
}

/// A compact single-choice control with icon-and-label options.
pub struct Segmented {
	props: Props,
	slot:  Slot,
	state: SegmentedState,
}

impl Segmented {
	/// Creates an empty segmented control.
	pub fn new() -> Self {
		Self { props: Props::new(), slot: next_slot(), state: SegmentedState::default() }
	}

	/// Sets one segmented-control property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		if prop == Prop::Value {
			self.sync_value();
		}
		self
	}

	/// Sets one segmented-control property from a string.
	pub fn with_str(mut self, prop: Prop, value: &str) -> Self {
		self.props.set(prop, value);
		if prop == Prop::Value {
			self.sync_value();
		}
		self
	}

	/// Appends an `<option>` and adopts its value, icon, and label metadata.
	pub fn option(mut self, option: SelectOption) -> Self {
		let (props, label) = option.into_control_parts();
		let icon = props.str_of(Prop::Icon).cloned().unwrap_or_default();
		let value = props
			.str_of(Prop::Value)
			.cloned()
			.or_else(|| (!label.is_empty()).then(|| label.clone()))
			.unwrap_or_else(|| icon.clone());
		self.state.options.push(OptionData { value, icon, label });
		self.sync_value();
		self
	}

	fn sync_value(&mut self) {
		let Some(value) = self.props.str_of(Prop::Value) else {
			return;
		};
		if let Some(index) = self
			.state
			.options
			.iter()
			.position(|option| option.value.as_str() == value.as_str())
		{
			self.state.idx = index as u16;
		}
	}

	fn option_width(option: &OptionData, ctx: &UiContext) -> u16 {
		let icon = ctx.charset.icon_named(&option.icon).unwrap_or(&option.icon);
		let gap = u16::from(!icon.is_empty() && !option.label.is_empty());
		2u16
			.saturating_add(cell_width(icon))
			.saturating_add(gap)
			.saturating_add(cell_width(&option.label))
	}
}

impl Default for Segmented {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for Segmented {
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
		let width = self
			.state
			.options
			.iter()
			.map(|option| Self::option_width(option, ctx))
			.fold(0u16, u16::saturating_add);
		(width, width)
	}

	fn height(&mut self, _ctx: &UiContext, _width: u16) -> u16 {
		1
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if rect.y >= pc.clip || rect.width == 0 || rect.height == 0 {
			return;
		}
		let focused = pc.focus == Some(self.slot);
		let hover_chip = match pc.hover {
			Some((slot, HitTag::Chip(index))) if slot == self.slot => Some(index),
			_ => None,
		};
		let mut x = rect.x;
		for (index, option) in self.state.options.iter().enumerate() {
			let index = index as u16;
			let start = x.saturating_sub(rect.x);
			let active = index == self.state.idx;
			let hovered = hover_chip == Some(index);
			let (background, foreground) =
				soft_colors(&pc.ctx.theme, pc.ctx.theme.accent, active, false, hovered);
			let mut style = Style::new().fg(foreground).bg(background);
			if active {
				style = style.bold();
			}
			if active && focused {
				style = style.underline();
			}
			x = pc.frame.put(x, rect.y, " ", style);
			let icon = pc
				.ctx
				.charset
				.icon_named(&option.icon)
				.unwrap_or(&option.icon);
			if !icon.is_empty() {
				x = pc.frame.put(x, rect.y, icon, style);
			}
			if !icon.is_empty() && !option.label.is_empty() {
				x = pc.frame.put(x, rect.y, " ", style);
			}
			if !option.label.is_empty() {
				x = pc.frame.put(x, rect.y, &option.label, style);
			}
			x = pc.frame.put(x, rect.y, " ", style);
			let end = x.saturating_sub(rect.x);
			pc.hits.push(Hit {
				rect: Rect::new(rect.x.saturating_add(start), rect.y, end.saturating_sub(start), 1),
				slot: self.slot,
				tag:  HitTag::Chip(index),
			});
		}
	}

	fn focusable(&self) -> bool {
		true
	}

	fn key(&mut self, _ec: &mut EventCtx<'_>, key: Key) -> Flow {
		let len = self.state.options.len() as u16;
		match key {
			Key::Left if len > 0 => {
				self.state.idx = (self.state.idx + len - 1) % len;
				Flow::Consumed
			},
			Key::Right if len > 0 => {
				self.state.idx = (self.state.idx + 1) % len;
				Flow::Consumed
			},
			_ => Flow::Skip,
		}
	}

	fn mouse(
		&mut self,
		_ec: &mut EventCtx<'_>,
		tag: HitTag,
		_at: (u16, u16),
		_rect: Rect,
		mouse: Mouse,
	) -> Flow {
		match mouse {
			Mouse::Click
				if let HitTag::Chip(index) = tag
					&& usize::from(index) < self.state.options.len() =>
			{
				self.state.idx = index;
				Flow::Consumed
			},
			Mouse::Click
			| Mouse::RightClick
			| Mouse::MiddleClick
			| Mouse::Move
			| Mouse::Drag
			| Mouse::Release
			| Mouse::WheelUp
			| Mouse::WheelDown
			| Mouse::WheelLeft
			| Mouse::WheelRight => Flow::Skip,
		}
	}

	fn value(&self, out: &mut serde_json::Map<String, serde_json::Value>) {
		let Some(id) = self.props.id() else {
			return;
		};
		let value = self
			.state
			.options
			.get(usize::from(self.state.idx))
			.map_or(serde_json::Value::Null, |option| {
				serde_json::Value::String(option.value.to_string())
			});
		out.insert(id.to_string(), value);
	}
}
