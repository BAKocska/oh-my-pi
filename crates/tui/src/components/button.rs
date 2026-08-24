use omp_core::{IntoStr, Str};
use strum::{EnumString, IntoStaticStr};

use crate::{
	component::{Component, EventCtx, Flow, Hit, HitTag, PaintCtx, Slot, next_slot},
	context::{Theme, UiContext},
	frame::{Color, Frame, Rect, Style},
	input::{Key, Mouse, UiEvent},
	props::{Prop, PropValue, Props},
	rich::cell_width,
};

/// Visual treatment applied by a [`Button`].
#[derive(Clone, Copy, Debug, EnumString, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "lowercase")]
pub enum ButtonVariant {
	/// Filled button with half-block end caps.
	Pill,
	/// Subtle neutral toggle chip, filled with its semantic color when active.
	Soft,
	/// Panel-adjacent semantic tint with a full-color label.
	Tint,
	/// Borderless text action.
	Ghost,
}

#[derive(Default)]
struct ButtonState {
	armed: bool,
}

/// A pressable action button.
pub struct Button {
	props: Props,
	slot:  Slot,
	state: ButtonState,
	label: Str,
}

impl Button {
	/// Creates an unlabeled button.
	pub fn new() -> Self {
		Self {
			props: Props::new(),
			slot:  next_slot(),
			state: ButtonState::default(),
			label: Str::default(),
		}
	}

	/// Sets one button property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self
	}

	/// Sets one button property from a string.
	pub fn with_str(mut self, prop: Prop, value: &str) -> Self {
		self.props.set(prop, value);
		self
	}

	/// Sets the button's text child.
	pub fn child(mut self, label: impl IntoStr) -> Self {
		let label = label.into_str();
		if self.label.is_empty() {
			self.label = label;
		} else {
			self.label = Str::from(format!("{}{}", self.label, label));
		}
		self
	}

	fn label(&self) -> &str {
		if !self.label.is_empty() {
			&self.label
		} else if let Some(label) = self.props.str_of(Prop::Label) {
			label
		} else if let Some(id) = self.props.id() {
			id
		} else {
			"ok"
		}
	}

	fn press(&mut self) -> Flow {
		if self.props.flag(Prop::Confirm) && !self.state.armed {
			self.state.armed = true;
			return Flow::Consumed;
		}
		self.state.armed = false;
		if self.props.flag(Prop::Cancel) {
			Flow::Event(UiEvent::Cancel)
		} else if self.props.flag(Prop::Submit) {
			Flow::Event(UiEvent::Submit)
		} else if let Some(id) = self.props.id() {
			Flow::Event(UiEvent::Pressed(id.clone()))
		} else {
			Flow::Event(UiEvent::Submit)
		}
	}

	fn variant(&self) -> Option<ButtonVariant> {
		self
			.props
			.str_of(Prop::Variant)
			.and_then(|value| value.parse().ok())
	}
}

/// Paints a filled pill with charset-selected end caps.
///
/// `highlight` is a focus treatment only. Callers derive hover fills from the
/// active theme's `hover` token before painting.
pub(super) fn paint_pill(
	frame: &mut Frame,
	x: u16,
	y: u16,
	label: &str,
	background: Color,
	foreground: Color,
	caps: (&str, &str),
	highlight: bool,
) -> u16 {
	let cap = Style::new().fg(background);
	let mut body = Style::new().fg(foreground).bg(background).bold();
	if highlight {
		body = body.underline();
	}
	let mut x = frame.put(x, y, caps.0, cap);
	x = frame.put(x, y, label, body);
	frame.put(x, y, caps.1, cap)
}

/// Returns theme-derived colors for a soft toggle chip.
pub(super) fn soft_colors(
	theme: &Theme,
	base: Color,
	active: bool,
	dim: bool,
	hovered: bool,
) -> (Color, Color) {
	let (mut background, mut foreground) = if active {
		(base, theme.contrast)
	} else {
		(theme.panel.mix(theme.fg, 0.10), theme.panel.mix(theme.fg, 0.62))
	};
	if dim {
		background = background.mix(theme.panel, 0.55);
		foreground = foreground.mix(theme.panel, 0.45);
	}
	if hovered {
		background = theme.hover;
		foreground = if active { base } else { theme.fg };
	}
	(background, foreground)
}

/// Paints a padded flat chip and returns the first cell after it.
pub(super) fn paint_chip(
	frame: &mut Frame,
	x: u16,
	y: u16,
	label: &str,
	background: Color,
	foreground: Color,
	dim: bool,
) -> u16 {
	let mut style = Style::new().fg(foreground).bg(background);
	if dim {
		style = style.dim();
	}
	let mut x = frame.put(x, y, " ", style);
	x = frame.put(x, y, label, style);
	frame.put(x, y, " ", style)
}

impl Default for Button {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for Button {
	fn props(&self) -> &Props {
		&self.props
	}

	fn props_mut(&mut self) -> &mut Props {
		&mut self.props
	}

	fn slot(&self) -> Slot {
		self.slot
	}

	fn measure(&mut self, _ctx: &UiContext) -> (u16, u16) {
		let chrome = match self.variant() {
			Some(ButtonVariant::Pill | ButtonVariant::Soft | ButtonVariant::Tint) => 2,
			Some(ButtonVariant::Ghost) => 0,
			None => 4,
		};
		let width = cell_width(self.label()).saturating_add(chrome);
		(width, width)
	}

	fn height(&mut self, _ctx: &UiContext, _width: u16) -> u16 {
		1
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if rect.y >= pc.clip {
			return;
		}
		pc.hits
			.push(Hit { rect, slot: self.slot, tag: HitTag::Press });
		let focused = pc.focus == Some(self.slot);
		let hovered = matches!(pc.hover, Some((slot, _)) if slot == self.slot);
		let active = self.props.flag(Prop::Active);
		let dim = self.props.flag(Prop::Dim);
		let base = self
			.props
			.color(Prop::Color, &pc.ctx.theme)
			.unwrap_or(pc.ctx.theme.accent);
		let text = if self.state.armed {
			"sure?"
		} else {
			self.label()
		};
		match self.variant() {
			Some(ButtonVariant::Pill) => {
				let mut background = if self.state.armed {
					pc.ctx.theme.warn
				} else if active {
					base.mix(pc.ctx.theme.fg, 0.22)
				} else if dim {
					base.mix(pc.ctx.theme.panel, 0.55)
				} else {
					base
				};
				if hovered {
					background = pc.ctx.theme.hover;
				}
				paint_pill(
					pc.frame,
					rect.x,
					rect.y,
					text,
					background,
					background.contrast_label(),
					pc.ctx.charset.pill_caps(),
					focused,
				);
			},
			Some(ButtonVariant::Soft) => {
				let (background, foreground) = soft_colors(&pc.ctx.theme, base, active, dim, hovered);
				paint_chip(pc.frame, rect.x, rect.y, text, background, foreground, dim);
			},
			Some(ButtonVariant::Tint) => {
				let mut background = pc.ctx.theme.tint_bg(base, 0.18);
				let mut foreground = base;
				if dim {
					background = background.mix(pc.ctx.theme.panel, 0.55);
					foreground = foreground.mix(pc.ctx.theme.panel, 0.45);
				}
				if hovered {
					background = pc.ctx.theme.hover;
				}
				paint_chip(pc.frame, rect.x, rect.y, text, background, foreground, dim);
			},
			Some(ButtonVariant::Ghost) => {
				let mut style = Style::new().fg(if active { base } else { pc.ctx.theme.fg });
				if hovered {
					style = style.bg(pc.ctx.theme.hover);
				}
				if focused {
					style = style.underline();
				}
				if dim {
					style = style.fg(pc.ctx.theme.muted).dim();
				}
				pc.frame.put(rect.x, rect.y, text, style);
			},
			None => {
				let accent = self.props.flag(Prop::Accent) || self.props.flag(Prop::Submit);
				let (mut background, foreground) = if self.state.armed {
					(pc.ctx.theme.warn, pc.ctx.theme.contrast)
				} else if accent {
					(pc.ctx.theme.accent, pc.ctx.theme.contrast)
				} else {
					(pc.ctx.theme.surface, pc.ctx.theme.fg)
				};
				if hovered {
					background = pc.ctx.theme.hover;
				}
				paint_pill(
					pc.frame,
					rect.x,
					rect.y,
					text,
					background,
					foreground,
					pc.ctx.charset.pill_caps(),
					focused,
				);
			},
		}
	}

	fn focusable(&self) -> bool {
		true
	}

	fn key(&mut self, _ec: &mut EventCtx<'_>, key: Key) -> Flow {
		match key {
			Key::Enter | Key::Space => self.press(),
			_ => {
				self.state.armed = false;
				Flow::Skip
			},
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
			Mouse::Click if tag == HitTag::Press => self.press(),
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
}

#[cfg(test)]
mod tests {
	use omp_core::sf;

	use super::*;
	use crate::{Frame, Size, test_support::frame_row_text};

	fn event_ctx(ctx: &UiContext) -> EventCtx<'_> {
		EventCtx::new(ctx, 16, 1)
	}

	#[test]
	fn enter_emits_matching_application_event() {
		let ctx = UiContext::default();
		let mut submit = Button::new().with(Prop::Submit, true).child("Go");
		assert_eq!(submit.key(&mut event_ctx(&ctx), Key::Enter), Flow::Event(UiEvent::Submit));
		let mut plain = Button::new().with(Prop::Id, "again").child("Again");
		assert_eq!(
			plain.key(&mut event_ctx(&ctx), Key::Enter),
			Flow::Event(UiEvent::Pressed(sf!("again")))
		);
	}

	#[test]
	fn confirm_arms_before_emitting() {
		let ctx = UiContext::default();
		let mut button = Button::new().with(Prop::Confirm, true);
		assert_eq!(button.key(&mut event_ctx(&ctx), Key::Enter), Flow::Consumed);
		assert_eq!(button.key(&mut event_ctx(&ctx), Key::Enter), Flow::Event(UiEvent::Submit));
	}

	#[test]
	fn paint_draws_label_and_press_hit() {
		let mut button = Button::new().child("Continue");
		let ctx = UiContext::default();
		let mut frame = Frame::new(Size::new(24, 1));
		let mut hits = Vec::new();
		let slot = button.slot();
		let mut wakes = Vec::new();
		let mut pc = PaintCtx::new(&mut frame, &ctx, &mut hits, &mut wakes);
		pc.focus = Some(slot);
		button.paint(&mut pc, Rect::new(0, 0, 24, 1));
		assert!(frame_row_text(&frame, 0).contains("Continue"));
		assert_eq!(hits[0].tag, HitTag::Press);
	}

	#[test]
	fn variants_parse_without_manual_string_tables() {
		assert_eq!("pill".parse(), Ok(ButtonVariant::Pill));
		assert_eq!("soft".parse(), Ok(ButtonVariant::Soft));
		assert_eq!("tint".parse(), Ok(ButtonVariant::Tint));
		assert_eq!("ghost".parse(), Ok(ButtonVariant::Ghost));
	}
}
