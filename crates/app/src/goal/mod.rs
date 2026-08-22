//! Durable autonomous goal lifecycle and budget reporting.

pub mod report;

use omp_core::Str;

use crate::modes::Goal;

/// Applied lifecycle transition returned to slash and tool callers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum GoalLifecycle {
	/// A new goal was created.
	Created,
	/// The latest projection was read without mutation.
	Current,
	/// A paused or limited goal resumed.
	Resumed,
	/// The objective was marked complete.
	Completed,
	/// The objective was abandoned.
	Dropped,
}

/// Typed result of one goal lifecycle operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalOutcome {
	/// Applied lifecycle operation.
	pub lifecycle: GoalLifecycle,
	/// Latest durable goal projection, absent only before creation.
	pub goal:      Option<Goal>,
}

/// Renders live goal context for a typed prompt slot.
pub fn prompt_context(goal: &Goal, todo: Option<&str>) -> Str {
	let budget = goal
		.token_budget
		.map_or_else(|| String::from("unbounded"), |value| value.to_string());
	let remaining = goal.token_budget.map_or_else(
		|| String::from("unbounded"),
		|value| value.saturating_sub(goal.tokens_used).to_string(),
	);
	let todo = todo
		.filter(|value| !value.trim().is_empty())
		.unwrap_or("none");
	Str::from(format!(
		"<goal-context>\n<objective>{}</objective>\n<tokens-used>{}</tokens-used>\\
		 n<token-budget>{budget}</token-budget>\n<remaining-tokens>{remaining}</remaining-tokens>\\
		 n<time-used-seconds>{}</time-used-seconds>\n<todo-context>{}</todo-context>\n</goal-context>",
		escape_xml(goal.objective.as_str()),
		goal.tokens_used,
		goal.time_used_seconds,
		escape_xml(todo),
	))
}

fn escape_xml(value: &str) -> String {
	value
		.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
}
