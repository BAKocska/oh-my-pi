//! Model-visible hard-budget completion reports.

use omp_core::{Str, sf};

use crate::modes::Goal;

/// Structured completion accounting for one goal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalBudgetReport {
	/// Fresh input, cache-write, and output tokens charged.
	pub charged_tokens: u64,
	/// Configured hard budget, or no limit.
	pub budget:         Option<u64>,
	/// Tokens remaining before the limit, or no limit.
	pub remaining:      Option<u64>,
	/// Tokens charged beyond the configured hard limit.
	pub overrun:        u64,
	/// Accounted wall-clock seconds.
	pub elapsed_secs:   u64,
}

impl GoalBudgetReport {
	/// Derives exact completion accounting from a goal projection.
	pub fn from_goal(goal: &Goal) -> Self {
		let remaining = goal
			.token_budget
			.map(|budget| budget.saturating_sub(goal.tokens_used));
		let overrun = goal
			.token_budget
			.map_or(0, |budget| goal.tokens_used.saturating_sub(budget));
		Self {
			charged_tokens: goal.tokens_used,
			budget: goal.token_budget,
			remaining,
			overrun,
			elapsed_secs: goal.time_used_seconds,
		}
	}

	/// Renders the instruction shown to the model on completion.
	pub fn model_prompt(&self) -> Str {
		let budget = self
			.budget
			.map_or_else(|| sf!("unbounded"), |value| sf!("{value}"));
		let remaining = self
			.remaining
			.map_or_else(|| sf!("unbounded"), |value| sf!("{value}"));
		sf!(
			"Goal achieved. Report final budget usage to the user: charged tokens: {}; configured \
			 budget: {}; remaining tokens: {}; overrun tokens: {}; elapsed time: {} seconds.",
			self.charged_tokens,
			budget,
			remaining,
			self.overrun,
			self.elapsed_secs,
		)
	}
}
