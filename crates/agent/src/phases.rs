#![allow(missing_docs, reason = "strum IntoStaticStr emits undocumented inherent methods")]

//! Stable lifecycle, invocation, and hook decision vocabularies.

/// Canonical lifecycle and invocation vocabularies.
pub use omp_core::phase::{ActivateReason, InvocationPhase, LifecyclePhase, RestartReason};
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

/// Ordered stage in the hook decision procedure.
#[allow(missing_docs, reason = "strum IntoStaticStr generates undocumented as_str")]
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	Ord,
	PartialEq,
	PartialOrd,
	Serialize,
)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", const_into_str)]
pub enum HookPhase {
	/// Pure, deterministic deny-only checks.
	Precheck  = 0,
	/// Totally ordered request transformations.
	Transform = 1,
	/// Parallel, budgeted review.
	Review    = 2,
	/// Approval requirements and final admission votes.
	Approval  = 3,
	/// Asynchronous observation after the outcome is fixed.
	Observe   = 4,
}

impl HookPhase {
	/// Every hook phase in decision-procedure order.
	pub const ALL: [Self; 5] =
		[Self::Precheck, Self::Transform, Self::Review, Self::Approval, Self::Observe];

	/// Returns the stable zero-based position in the hook procedure.
	#[must_use]
	pub const fn ordinal(self) -> u8 {
		self as u8
	}
}

/// Canonical answer returned by a gateable hook.
///
/// This enum is only the stable arm discriminator and phase-legality matrix.
/// Per-arm payloads belong to the later hook and wire contracts.
#[allow(missing_docs, reason = "strum IntoStaticStr generates undocumented as_str")]
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	PartialEq,
	Serialize,
)]
#[repr(u8)]
#[serde(rename_all = "PascalCase")]
#[strum(serialize_all = "PascalCase", const_into_str)]
pub enum HookDecision {
	/// Cast an affirmative vote.
	Allow           = 0,
	/// Refuse the operation.
	Deny            = 1,
	/// Replace or patch the mutable request fields.
	Modify          = 2,
	/// Abstain without changing the procedure.
	Defer           = 3,
	/// Ask Core to create or merge a durable approval requirement.
	RequireApproval = 4,
}

impl HookDecision {
	/// Every hook decision arm in canonical vocabulary order.
	pub const ALL: [Self; 5] =
		[Self::Allow, Self::Deny, Self::Modify, Self::Defer, Self::RequireApproval];

	/// Returns whether this decision is legal in `phase`.
	#[must_use]
	pub const fn is_legal_in(self, phase: HookPhase) -> bool {
		matches!(
			(phase, self),
			(HookPhase::Precheck, Self::Deny | Self::Defer)
				| (HookPhase::Transform, Self::Modify | Self::Defer)
				| (HookPhase::Review, Self::Allow | Self::Deny | Self::Defer)
				| (HookPhase::Approval, Self::Allow | Self::Deny | Self::Defer | Self::RequireApproval)
				| (HookPhase::Observe, Self::Defer)
		)
	}
}

#[cfg(test)]
mod tests {
	use super::{HookDecision, HookPhase, InvocationPhase, LifecyclePhase};

	#[test]
	fn invocation_phase_order_and_discriminants_are_exact() {
		assert_eq!(InvocationPhase::ALL, [
			InvocationPhase::Open,
			InvocationPhase::ArgsFinalized,
			InvocationPhase::Admission,
			InvocationPhase::Admitted,
			InvocationPhase::AssistantItemCommitted,
			InvocationPhase::EffectsAuthorized,
			InvocationPhase::Settled,
		]);
		for (ordinal, phase) in InvocationPhase::ALL.into_iter().enumerate() {
			assert_eq!(phase.ordinal(), ordinal as u8);
		}
	}

	#[test]
	fn only_settled_is_terminal_and_it_has_no_successor() {
		for phase in InvocationPhase::ALL {
			assert_eq!(phase.is_terminal(), phase == InvocationPhase::Settled);
		}
		assert!(!InvocationPhase::Settled.can_transition_to(InvocationPhase::Settled));
		assert!(!InvocationPhase::Settled.can_transition_to(InvocationPhase::Open));
	}

	#[test]
	fn invocation_transitions_are_adjacent_only() {
		for from in InvocationPhase::ALL {
			for to in InvocationPhase::ALL {
				let expected = !from.is_terminal() && to.ordinal() == from.ordinal() + 1;
				assert_eq!(from.can_transition_to(to), expected, "{from} -> {to}");
			}
		}
	}

	#[test]
	fn operation_phase_legality_observes_both_boundaries() {
		assert!(!InvocationPhase::ArgsFinalized.allows_operation(InvocationPhase::Admission));
		assert!(InvocationPhase::Admission.allows_operation(InvocationPhase::Admission));
		assert!(
			InvocationPhase::EffectsAuthorized
				.allows_operation(InvocationPhase::AssistantItemCommitted)
		);
		assert!(!InvocationPhase::Settled.allows_operation(InvocationPhase::Open));
	}

	#[test]
	fn hook_decision_legality_matrix_is_exact() {
		const EXPECTED: [[bool; 5]; 5] = [
			[false, true, false, true, false],
			[false, false, true, true, false],
			[true, true, false, true, false],
			[true, true, false, true, true],
			[false, false, false, true, false],
		];

		for phase in HookPhase::ALL {
			for decision in HookDecision::ALL {
				assert_eq!(
					decision.is_legal_in(phase),
					EXPECTED[phase.ordinal() as usize][decision as usize],
					"{decision} in {phase}"
				);
			}
		}
	}

	#[test]
	fn string_vocabularies_are_exact() {
		for (phase, name) in InvocationPhase::ALL.into_iter().zip([
			"OPEN",
			"ARGS_FINALIZED",
			"ADMISSION",
			"ADMITTED",
			"ASSISTANT_ITEM_COMMITTED",
			"EFFECTS_AUTHORIZED",
			"SETTLED",
		]) {
			assert_eq!(phase.to_string(), name);
			assert_eq!(name.parse::<InvocationPhase>(), Ok(phase));
		}
		for (phase, name) in
			HookPhase::ALL
				.into_iter()
				.zip(["precheck", "transform", "review", "approval", "observe"])
		{
			assert_eq!(phase.to_string(), name);
			assert_eq!(name.parse::<HookPhase>(), Ok(phase));
		}
		for (decision, name) in
			HookDecision::ALL
				.into_iter()
				.zip(["Allow", "Deny", "Modify", "Defer", "RequireApproval"])
		{
			assert_eq!(decision.to_string(), name);
			assert_eq!(name.parse::<HookDecision>(), Ok(decision));
		}
		for (phase, name) in LifecyclePhase::ALL
			.into_iter()
			.zip(["DECLARED", "FROZEN", "VERIFIED", "ACTIVE", "DEGRADED"])
		{
			assert_eq!(phase.to_string(), name);
			assert_eq!(name.parse::<LifecyclePhase>(), Ok(phase));
		}
	}

	#[test]
	fn auxiliary_phase_orders_are_exact() {
		assert_eq!(HookPhase::ALL, [
			HookPhase::Precheck,
			HookPhase::Transform,
			HookPhase::Review,
			HookPhase::Approval,
			HookPhase::Observe,
		]);
		assert_eq!(LifecyclePhase::ALL, [
			LifecyclePhase::Declared,
			LifecyclePhase::Frozen,
			LifecyclePhase::Verified,
			LifecyclePhase::Active,
			LifecyclePhase::Degraded,
		]);
	}
}
