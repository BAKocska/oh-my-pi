//! Shared ordered protocol phase vocabularies.
#![allow(missing_docs, reason = "strum IntoStaticStr emits undocumented inherent methods")]

use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

/// Canonical ordered state of one tool invocation.
///
/// Each transition fixes additional durable facts. Discriminants are stable
/// protocol vocabulary and therefore match the state-machine order.
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
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE", const_into_str)]
pub enum InvocationPhase {
	/// Streaming has named the target, but argument emission remains open.
	Open              = 0,
	/// The requested target and canonical requested arguments are fixed.
	ArgsFinalized     = 1,
	/// Admission policy is evaluating the finalized request.
	Admission         = 2,
	/// Policy has fixed the effective arguments and admission receipt.
	Admitted          = 3,
	/// The assistant item containing this invocation is durable.
	AssistantItemCommitted = 4,
	/// Core has issued the invocation's scoped effect token.
	EffectsAuthorized = 5,
	/// The single durable call outcome is fixed.
	Settled           = 6,
}

impl InvocationPhase {
	/// Every invocation phase in canonical transition order.
	pub const ALL: [Self; 7] = [
		Self::Open,
		Self::ArgsFinalized,
		Self::Admission,
		Self::Admitted,
		Self::AssistantItemCommitted,
		Self::EffectsAuthorized,
		Self::Settled,
	];

	/// Returns the stable zero-based protocol discriminant.
	#[must_use]
	pub const fn ordinal(self) -> u8 {
		self as u8
	}

	/// Returns whether this phase is terminal.
	#[must_use]
	pub const fn is_terminal(self) -> bool {
		matches!(self, Self::Settled)
	}

	/// Returns whether a direct transition from `self` to `next` is legal.
	#[must_use]
	pub const fn can_transition_to(self, next: Self) -> bool {
		self.ordinal() + 1 == next.ordinal()
	}

	/// Returns whether this invocation has reached `required`.
	#[must_use]
	pub const fn has_reached(self, required: Self) -> bool {
		self.ordinal() >= required.ordinal()
	}

	/// Returns whether an operation with `minimum` phase may run now.
	///
	/// Settled invocations cannot start new work even when they reached the
	/// operation's minimum phase earlier.
	#[must_use]
	pub const fn allows_operation(self, minimum: Self) -> bool {
		!self.is_terminal() && self.has_reached(minimum)
	}
}

#[cfg(test)]
mod tests {
	use super::InvocationPhase;

	#[test]
	fn discriminants_and_transitions_are_canonical() {
		for (ordinal, phase) in InvocationPhase::ALL.into_iter().enumerate() {
			assert_eq!(usize::from(phase.ordinal()), ordinal);
			assert_eq!(phase.is_terminal(), phase == InvocationPhase::Settled);
		}
		for pair in InvocationPhase::ALL.windows(2) {
			assert!(pair[0].can_transition_to(pair[1]));
		}
		assert!(!InvocationPhase::Open.can_transition_to(InvocationPhase::Admission));
		assert!(!InvocationPhase::Settled.can_transition_to(InvocationPhase::Settled));
	}

	#[test]
	fn operation_gate_requires_minimum_and_nonterminal_phase() {
		assert!(!InvocationPhase::Admitted.allows_operation(InvocationPhase::EffectsAuthorized));
		assert!(
			InvocationPhase::EffectsAuthorized.allows_operation(InvocationPhase::EffectsAuthorized)
		);
		assert!(!InvocationPhase::Settled.allows_operation(InvocationPhase::Open));
	}
}
