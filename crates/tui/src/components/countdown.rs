//! Reusable presentation-clock countdown for dialogs and approval prompts.

use std::time::Duration;

use omp_core::{Str, fmts};

use crate::{
	component::{Component, PaintCtx, Slot, next_slot},
	context::UiContext,
	frame::Rect,
	props::Props,
};

/// One allocation-free countdown driven by [`UiContext::now`].
pub struct Countdown {
	props:    Props,
	slot:     Slot,
	label:    Str,
	started:  Duration,
	duration: Duration,
}

impl Countdown {
	/// Creates a countdown beginning at presentation time `started`.
	#[must_use]
	pub fn new(label: impl Into<Str>, started: Duration, duration: Duration) -> Self {
		Self { props: Props::new(), slot: next_slot(), label: label.into(), started, duration }
	}

	/// Returns the remaining whole seconds, rounding a partial second up.
	#[must_use]
	pub fn remaining(&self, now: Duration) -> u64 {
		let left = self
			.duration
			.saturating_sub(now.saturating_sub(self.started));
		let millis = left.as_millis();
		u64::try_from(millis.saturating_add(999) / 1000).unwrap_or(u64::MAX)
	}

	/// Reports whether the deadline has elapsed.
	#[must_use]
	pub fn expired(&self, now: Duration) -> bool {
		now.saturating_sub(self.started) >= self.duration
	}
}

impl Component for Countdown {
	fn props(&self) -> &Props {
		&self.props
	}

	fn props_mut(&mut self) -> &mut Props {
		&mut self.props
	}

	fn slot(&self) -> Slot {
		self.slot
	}

	fn paints_border(&self) -> bool {
		false
	}

	fn measure(&mut self, _ctx: &UiContext) -> (u16, u16) {
		let width = xutf::width_str(&self.label).saturating_add(8);
		(1, u16::try_from(width).unwrap_or(u16::MAX))
	}

	fn height(&mut self, _ctx: &UiContext, _width: u16) -> u16 {
		1
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		let remaining = self.remaining(pc.ctx.now);
		let text = fmts!("{} · {remaining}s", self.label);
		let color = if remaining <= 5 {
			pc.ctx.theme.err
		} else {
			pc.ctx.theme.warn
		};
		pc.frame
			.put(rect.x, rect.y, &text, self.props.style(&pc.ctx.theme).fg(color));
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rounds_partial_seconds_and_expires_at_deadline() {
		let countdown =
			Countdown::new("Retrying", Duration::from_secs(10), Duration::from_millis(2500));
		assert_eq!(countdown.remaining(Duration::from_secs(10)), 3);
		assert_eq!(countdown.remaining(Duration::from_secs(12)), 1);
		assert!(!countdown.expired(Duration::from_millis(12_499)));
		assert!(countdown.expired(Duration::from_millis(12_500)));
	}
}
