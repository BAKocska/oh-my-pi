//! Compaction retention predicate for canonical plan reads.

use std::collections::BTreeSet;

use omp_core::Str;

use super::{artifacts::canonical_url, state::DEFAULT_PLAN_URL};

/// Dynamic matcher protecting read outcomes for the canonical and active plans.
#[derive(Clone, Debug)]
pub struct PlanReadProtection {
	active: Str,
}

impl PlanReadProtection {
	/// Creates a matcher for the current active plan reference.
	pub fn new(active: impl Into<Str>) -> Self {
		Self { active: active.into() }
	}

	/// Replaces the active plan reference after approval or re-entry.
	pub fn set_active(&mut self, active: impl Into<Str>) {
		self.active = active.into();
	}

	/// Returns whether a completed tool outcome must survive every compaction
	/// method. Only `read` outcomes are eligible.
	pub fn retains(&self, tool: &str, path: &str) -> bool {
		tool == "read" && (targets(path, DEFAULT_PLAN_URL) || targets(path, self.active.as_str()))
	}
}

/// Event-index projection consumed by
/// `omp_agent::plan_lossless_with_warm_suffix`.
#[derive(Clone, Debug, Default)]
pub struct PlanReadRetention {
	events: BTreeSet<u64>,
}

impl PlanReadRetention {
	/// Records one projected tool result when it reads the active plan.
	pub fn observe(&mut self, event: u64, protection: &PlanReadProtection, tool: &str, path: &str) {
		if protection.retains(tool, path) {
			self.events.insert(event);
		}
	}

	/// Forgets an event after its branch is no longer reachable.
	pub fn remove(&mut self, event: u64) {
		self.events.remove(&event);
	}

	/// Returns the zero-allocation predicate passed through prune, shake, and
	/// asynchronous lossless compaction planning.
	pub fn predicate(&self) -> impl Fn(u64) -> bool + '_ {
		|event| self.events.contains(&event)
	}
}

fn targets(read: &str, target: &str) -> bool {
	let Ok(read) = canonical_url(read) else {
		return false;
	};
	let Ok(target) = canonical_url(target) else {
		return false;
	};
	read == target
		|| read
			.strip_prefix(target.as_str())
			.is_some_and(|suffix| suffix.starts_with(':'))
}
