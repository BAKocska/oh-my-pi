//! Durable plan-mode projection.

use omp_core::{Str, sf};
use serde::{Deserialize, Serialize};

/// Canonical initial plan artifact.
pub const DEFAULT_PLAN_URL: &str = "local://PLAN.md";

/// Approved-plan execution topology.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Eq,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum PlanWorkflow {
	/// Execute independent plan segments concurrently when dependencies permit.
	#[default]
	Parallel,
	/// Execute the plan one segment at a time.
	Iterative,
}

/// Session-local plan state folded from the canonical transcript journal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanState {
	/// Whether plan-mode restrictions and prompts are active.
	pub enabled:  bool,
	/// Canonical `local://` reference to the active plan artifact.
	pub artifact: Str,
	/// Workflow requested for approved-plan execution.
	pub workflow: PlanWorkflow,
	/// Whether this entry returned to an existing planning session.
	pub reentry:  bool,
}

impl Default for PlanState {
	fn default() -> Self {
		Self {
			enabled:  false,
			artifact: sf!(DEFAULT_PLAN_URL),
			workflow: PlanWorkflow::Parallel,
			reentry:  false,
		}
	}
}

impl PlanState {
	/// Creates the next enabled projection, preserving the active artifact and
	/// workflow while recording re-entry.
	pub fn entered(previous: Option<&Self>) -> Self {
		let mut state = previous.cloned().unwrap_or_default();
		state.reentry = previous.is_some();
		state.enabled = true;
		state
	}

	/// Returns the disabled projection without discarding the plan reference.
	pub fn exited(&self) -> Self {
		let mut state = self.clone();
		state.enabled = false;
		state
	}
}
