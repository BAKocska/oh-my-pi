//! Bounded transient progress for operator-facing maintenance commands.

use std::{io::IsTerminal as _, time::Duration};

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

/// A TTY-aware bounded progress handle.
pub struct ProgressReporter {
	bar: ProgressBar,
}

impl ProgressReporter {
	/// Creates a bounded reporter. Quiet and non-TTY callers receive a hidden
	/// draw target while retaining the same accounting contract.
	pub fn bounded(length: u64, message: impl Into<String>, quiet: bool) -> Self {
		let bar = if quiet || !std::io::stderr().is_terminal() {
			ProgressBar::hidden()
		} else {
			ProgressBar::with_draw_target(Some(length), ProgressDrawTarget::stderr_with_hz(12))
		};
		bar.set_style(
			ProgressStyle::with_template("{spinner:.cyan} {msg} [{bar:32.cyan/blue}] {pos}/{len}")
				.expect("static progress template"),
		);
		bar.enable_steady_tick(Duration::from_millis(90));
		bar.set_message(message.into());
		Self { bar }
	}

	/// Advances without exceeding the declared bound.
	pub fn advance(&self, delta: u64) {
		let next = self
			.bar
			.position()
			.saturating_add(delta)
			.min(self.bar.length().unwrap_or(u64::MAX));
		self.bar.set_position(next);
	}

	/// Completes and clears the transient line.
	pub fn finish(self) {
		self.bar.finish_and_clear();
	}
}
