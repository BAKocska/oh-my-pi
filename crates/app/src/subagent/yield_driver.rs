//! Bounded subagent yield enforcement state machine.

use omp_core::{Str, sf};

/// Standard prefix when a caller schema permissively overrides invalid output.
pub const WARNING_SCHEMA_OVERRIDDEN: &str =
	"[subagent schema overridden] output did not satisfy the effective schema";
/// Standard prefix when a yield explicitly contains null data.
pub const WARNING_NULL_YIELD: &str = "[subagent null yield] no usable structured data was returned";
/// Standard prefix when a run ends without a yield.
pub const WARNING_MISSING_YIELD: &str =
	"[subagent missing yield] the run ended without finalization";
/// Maximum invalid yield calls before correction stops.
pub const MAX_INVALID_YIELD_CALLS: u8 = 6;
/// Maximum omission reminders before forcing the tool choice.
pub const MAX_OMISSION_REMINDERS: u8 = 2;

/// Action selected after a model step or asynchronous child delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum YieldDirective {
	/// A valid terminal yield completed the run.
	Complete,
	/// Continue normally; incremental sections remain latched.
	Continue,
	/// Reprompt with a bounded correction message.
	Reprompt(Str),
	/// Restrict the next assistant step to the yield tool.
	ForceYield(Str),
	/// Stop enforcement because the provider returned a terminal error.
	TerminalModelError,
	/// The bounded ladder was exhausted without a valid yield.
	Missing,
}

/// Per-generation yield enforcement owner.
#[derive(Clone, Debug, Default)]
pub struct YieldDriver {
	required:           bool,
	terminal:           bool,
	incremental:        bool,
	pending_children:   usize,
	omission_reminders: u8,
	invalid_calls:      u8,
}

impl YieldDriver {
	/// Creates enforcement for a schema-bound or otherwise required result.
	#[must_use]
	pub const fn new(required: bool) -> Self {
		Self {
			required,
			terminal: false,
			incremental: false,
			pending_children: 0,
			omission_reminders: 0,
			invalid_calls: 0,
		}
	}

	/// Records a valid incremental or terminal yield call.
	pub fn accepted(&mut self, incremental: bool) -> YieldDirective {
		if incremental {
			self.incremental = true;
			YieldDirective::Continue
		} else {
			self.terminal = true;
			YieldDirective::Complete
		}
	}

	/// Records one invalid call and returns the next correction action.
	pub fn invalid(&mut self, reason: &str) -> YieldDirective {
		self.invalid_calls = self.invalid_calls.saturating_add(1);
		if self.invalid_calls >= MAX_INVALID_YIELD_CALLS {
			return YieldDirective::Missing;
		}
		YieldDirective::Reprompt(sf!(
			"Invalid yield call ({}/{}): {}. Correct the payload and call yield again.",
			self.invalid_calls,
			MAX_INVALID_YIELD_CALLS,
			reason
		))
	}

	/// Updates the count of owned asynchronous child jobs.
	pub const fn set_pending_children(&mut self, count: usize) {
		self.pending_children = count;
	}

	/// Un-latches a stale terminal choice when an async result is delivered.
	pub fn async_delivered(&mut self) {
		if self.terminal {
			self.terminal = false;
			self.omission_reminders = 0;
		}
		self.pending_children = self.pending_children.saturating_sub(1);
	}

	/// Chooses finalization behavior after a provider step stops.
	pub fn on_stop(&mut self, terminal_model_error: bool) -> YieldDirective {
		if terminal_model_error {
			return YieldDirective::TerminalModelError;
		}
		if self.terminal {
			return YieldDirective::Complete;
		}
		if self.pending_children != 0 {
			return YieldDirective::Reprompt(sf!(
				"{} owned subagent job(s) are still pending. Wait for delivery or cancel them before \
				 finalizing.",
				self.pending_children
			));
		}
		if !self.required && !self.incremental {
			return YieldDirective::Continue;
		}
		if self.omission_reminders < MAX_OMISSION_REMINDERS {
			self.omission_reminders = self.omission_reminders.saturating_add(1);
			return YieldDirective::Reprompt(sf!(
				"Finalize through yield now ({}/{} reminders). Include complete result.data or an \
				 explicit error.",
				self.omission_reminders,
				MAX_OMISSION_REMINDERS
			));
		}
		if self.omission_reminders == MAX_OMISSION_REMINDERS {
			self.omission_reminders = self.omission_reminders.saturating_add(1);
			return YieldDirective::ForceYield(sf!(
				"Call yield now. No other tool is available for this final step."
			));
		}
		YieldDirective::Missing
	}
}
