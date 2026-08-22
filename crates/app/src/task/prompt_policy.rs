//! Pure task-policy classification frozen before system-prompt rendering.

use omp_agent::EagerTaskPolicy;

use crate::prompt_prep::DelegationPromptInput;

/// Immutable inputs from the task supervisor and settings snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TaskPromptPolicyInput {
	/// Whether delegation is mounted and granted.
	pub enabled:         bool,
	/// Eager delegation setting.
	pub eager:           EagerTaskPolicy,
	/// Whether one task call accepts a batch.
	pub batch:           bool,
	/// Tree-wide live concurrency cap; zero means unlimited.
	pub concurrency:     u32,
	/// Requests already waiting for admission.
	pub queued:          u32,
	/// Whether the read-only scout role is available.
	pub scout_available: bool,
	/// Whether peer coordination is available.
	pub coordination:    bool,
}

/// Freezes task policy and the GPT-5.6-specific wording classification.
///
/// This function performs no model or supervisor I/O. Callers must supply one
/// already-resolved model identifier and one immutable supervisor snapshot.
#[must_use]
pub fn freeze(model_id: &str, input: TaskPromptPolicyInput) -> DelegationPromptInput {
	DelegationPromptInput {
		enabled:         input.enabled,
		concurrency:     input.concurrency,
		queued:          input.queued,
		scout_available: input.scout_available,
		eager:           input.eager,
		batch:           input.batch,
		coordination:    input.coordination,
		codex:           uses_codex_task_prompt(model_id),
	}
}

/// Whether task guidance uses the Codex GPT-5.6 delegation policy.
///
/// Provider qualification, thinking selectors, and route suffixes do not
/// change the underlying model version. GPT-5.60 and other versions do not
/// match the 5.6 policy.
#[must_use]
pub fn uses_codex_task_prompt(model_id: &str) -> bool {
	let bare = model_id.rsplit('/').next().unwrap_or(model_id);
	let bare = bare.split([':', '@']).next().unwrap_or(bare);
	let Some(suffix) = bare.strip_prefix("gpt-5.6") else {
		return false;
	};
	suffix.is_empty() || suffix.starts_with('-')
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn codex_policy_matches_only_gpt_5_6_version_family() {
		for model in [
			"gpt-5.6",
			"openai-codex/gpt-5.6-sol",
			"openai/gpt-5.6-terra:max",
			"openai-codex/gpt-5.6-luna@vercel-gw",
		] {
			assert!(uses_codex_task_prompt(model), "{model}");
		}
		for model in ["", "gpt-5.5", "gpt-5.60", "claude-gpt-5.6", "gpt-5.7"] {
			assert!(!uses_codex_task_prompt(model), "{model}");
		}
	}

	#[test]
	fn freeze_retains_live_queue_and_eager_policy() {
		let snapshot = freeze("openai-codex/gpt-5.6-sol", TaskPromptPolicyInput {
			enabled:         true,
			eager:           EagerTaskPolicy::Always,
			batch:           true,
			concurrency:     8,
			queued:          2,
			scout_available: true,
			coordination:    true,
		});
		assert!(snapshot.codex);
		assert_eq!(snapshot.eager, EagerTaskPolicy::Always);
		assert_eq!(snapshot.concurrency, 8);
		assert_eq!(snapshot.queued, 2);
		assert!(snapshot.batch);
		assert!(snapshot.coordination);
	}
}
